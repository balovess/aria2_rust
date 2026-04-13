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
    peers: Vec<PeerStats>,
    config: ChokingConfig,
    /// Explicitly snubbed peer indices (separate from PeerStats.is_snubbed for
    /// algorithm-level control). Peers in this set always receive score -1000.
    snubbed_peers: HashSet<usize>,
    /// Index of the current optimistically unchoked peer (for rotation).
    current_optimistic_peer: Option<usize>,
    /// Round-robin counter for optimistic unchoke rotation.
    optimistic_rotation_counter: usize,
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
        // Step 1: Check and mark snubbed peers
        self.check_snubbed_peers_internal();

        if self.peers.is_empty() {
            return vec![];
        }

        let max_slots = self.config.max_upload_slots;

        // Step 2: Calculate scores and sort indices by score descending
        let mut scored_peers: Vec<(usize, f64)> = self
            .peers
            .iter()
            .enumerate()
            .map(|(i, peer)| {
                let is_snubbed = self.snubbed_peers.contains(&i);
                (i, Self::calculate_peer_score(peer, is_snubbed))
            })
            .collect();

        // Sort by score descending
        scored_peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Step 3 & 4: Determine which peers should be unchoked vs choked
        let mut actions = Vec::new();
        let mut new_unchoked_indices = std::collections::HashSet::new();

        // Top K peers should be unchoked
        for (rank, &(idx, _)) in scored_peers.iter().enumerate() {
            if rank < max_slots {
                // Should be unchoked
                if self.peers[idx].am_choking {
                    actions.push(ChokeAction::Unchoke(idx));
                    self.peers[idx].record_unchoke();
                } else {
                    actions.push(ChokeAction::NoChange(idx));
                }
                new_unchoked_indices.insert(idx);
            } else {
                // Should be choked
                if !self.peers[idx].am_choking {
                    actions.push(ChokeAction::Choke(idx));
                    self.peers[idx].record_choke();
                } else {
                    actions.push(ChokeAction::NoChange(idx));
                }
            }
        }

        actions
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
        // Find candidates that are:
        //   - Currently choked (am_choking == true)
        //   - Interested in us (peer_interested == true)
        //   - Not snubbed (neither PeerStats.is_snubbed nor in explicit set)
        //   - Not recently optimistically unchoked (>interval ago)
        let candidates: Vec<usize> = self
            .peers
            .iter()
            .enumerate()
            .filter(|(i, peer)| {
                peer.am_choking
                    && peer.peer_interested
                    && !peer.is_snubbed
                    && !self.snubbed_peers.contains(i)
                    && peer.time_since_last_optimistic_unchoke().as_secs()
                        >= self.config.optimistic_unchoke_interval_secs
            })
            .map(|(i, _)| i)
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Use round-robin selection: pick next candidate after current position
        let selected = self.rotate_optimistic_unchoked(&candidates);

        // Mark as optimistically unchoked
        if let Some(peer) = self.peers.get_mut(selected) {
            peer.record_optimistic_unchoke();
        }
        self.current_optimistic_peer = Some(selected);

        Some(selected)
    }

    /// Rotate which peer gets the optimistic unchoke slot using round-robin.
    ///
    /// Picks a different peer than the current one when possible,
    /// cycling through eligible peers in order.
    ///
    /// # Arguments
    /// * `eligible_peers` - Indices of peers that are eligible for optimistic unchoke
    ///
    /// # Returns
    /// The index of the selected peer from the eligible set
    pub fn rotate_optimistic_unchoked(&mut self, eligible_peers: &[usize]) -> usize {
        if eligible_peers.is_empty() {
            panic!("rotate_optimistic_unchoked called with empty eligible list");
        }

        if eligible_peers.len() == 1 {
            return eligible_peers[0];
        }

        // Find position of current optimistic peer in eligible list
        let current_pos = self
            .current_optimistic_peer
            .and_then(|curr| eligible_peers.iter().position(|&x| x == curr));

        // Advance to next peer in round-robin order
        let next_pos = match current_pos {
            Some(pos) => (pos + 1) % eligible_peers.len(),
            None => self.optimistic_rotation_counter % eligible_peers.len(),
        };

        self.optimistic_rotation_counter = self.optimistic_rotation_counter.wrapping_add(1);
        eligible_peers[next_pos]
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
    /// This adds the peer to the `snubbed_peers` set, which causes
    /// `calculate_peer_score` to return -1000 for this peer, ensuring
    /// they always get choked on the next rotation.
    pub fn mark_peer_snubbed(&mut self, peer_id: usize) {
        if self.snubbed_peers.insert(peer_id) {
            tracing::debug!("[BT] Peer {} explicitly marked as snubbed", peer_id);
        }
    }

    /// Remove a peer from the explicit snubbed set (they sent data again).
    ///
    /// Returns `true` if the peer was actually in the snubbed set (newly un-snubbed),
    /// `false` if they were not snubbed.
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
        self.check_snubbed_peers_internal()
    }

    /// Internal implementation of snubbed checking
    fn check_snubbed_peers_internal(&mut self) -> Vec<usize> {
        let mut snubbed = vec![];
        for (i, peer) in self.peers.iter_mut().enumerate() {
            if peer.check_snubbed(self.config.snubbed_timeout_secs) {
                snubbed.push(i);
            }
        }
        snubbed
    }

    /// Score function: higher = better peer to keep unchoked
    ///
    /// Score components:
    ///   1. Download speed contribution (how much they give us): weight 0.5
    ///   2. Upload speed contribution (reciprocity): weight 0.3
    ///   3. Snubbed penalty: -1000 if snubbed (either in PeerStats or algorithm set)
    ///   4. Interest bonus: +50 if peer_interested
    ///   5. New peer bonus (time since unchoke < 60s): +30 (anti-churn)
    fn calculate_peer_score(peer: &PeerStats, is_explicitly_snubbed: bool) -> f64 {
        let mut score = 0.0;

        // Download speed (primary factor - tit-for-tat)
        // Scale down to reasonable range
        score += peer.download_speed * 0.00001;

        // Upload speed (reciprocity)
        score += peer.upload_speed * 0.000005;

        // Snubbed penalty (heavy penalty to avoid wasting slots)
        // Check both PeerStats-level and algorithm-level snubbing
        if peer.is_snubbed || is_explicitly_snubbed {
            score -= 1000.0;
        }

        // Interest bonus (prefer peers who want our data)
        if peer.peer_interested {
            score += 50.0;
        }

        // Anti-churn: prefer keeping current unchoked peers stable
        if !peer.am_choking && peer.time_since_last_unchoke().as_secs() < 60 {
            score += 30.0;
        }

        score
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
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// Helper to create a test peer with specific characteristics
    fn create_test_peer(
        download_speed: f64,
        upload_speed: f64,
        am_choking: bool,
        peer_interested: bool,
    ) -> PeerStats {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let mut peer = PeerStats::new([0u8; 20], addr);
        peer.download_speed = download_speed;
        peer.upload_speed = upload_speed;
        peer.am_choking = am_choking;
        peer.peer_interested = peer_interested;
        peer
    }

    #[test]
    fn test_new_algorithm_empty() {
        let config = ChokingConfig::default();
        let algo = ChokingAlgorithm::new(config);

        assert!(algo.is_empty());
        assert_eq!(algo.len(), 0);
    }

    #[test]
    fn test_add_remove_peers() {
        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        // Add peers
        assert_eq!(algo.len(), 0);
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        algo.add_peer(PeerStats::new([0u8; 20], addr));
        assert_eq!(algo.len(), 1);
        algo.add_peer(PeerStats::new([0u8; 20], addr));
        assert_eq!(algo.len(), 2);
        algo.add_peer(PeerStats::new([0u8; 20], addr));
        assert_eq!(algo.len(), 3);

        // Remove middle peer
        algo.remove_peer(1);
        assert_eq!(algo.len(), 2);

        // Remove first peer
        algo.remove_peer(0);
        assert_eq!(algo.len(), 1);

        // Remove last peer
        algo.remove_peer(0);
        assert!(algo.is_empty());
    }

    #[test]
    fn test_rotate_choke_selects_top_k() {
        let config = ChokingConfig {
            max_upload_slots: 3,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add 6 peers with different speeds (all start choked)
        // Peer 0: highest download speed
        algo.add_peer(create_test_peer(100000.0, 1000.0, true, true));
        // Peer 1: medium-high
        algo.add_peer(create_test_peer(80000.0, 800.0, true, true));
        // Peer 2: medium
        algo.add_peer(create_test_peer(60000.0, 600.0, true, true));
        // Peer 3: medium-low
        algo.add_peer(create_test_peer(40000.0, 400.0, true, true));
        // Peer 4: low
        algo.add_peer(create_test_peer(20000.0, 200.0, true, true));
        // Peer 5: very low
        algo.add_peer(create_test_peer(10000.0, 100.0, true, true));

        let actions = algo.rotate_choke();

        // Count unchoke actions
        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();

        // Should have exactly 3 unchoke actions (top 3 by score)
        assert_eq!(unchoke_count, 3);

        // Verify all actions are accounted for
        assert_eq!(actions.len(), 6); // One action per peer
    }

    #[test]
    fn test_rotate_choke_minimizes_changes() {
        let config = ChokingConfig {
            max_upload_slots: 2,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add 4 peers
        algo.add_peer(create_test_peer(100000.0, 1000.0, false, true)); // Already unchoked, high speed
        algo.add_peer(create_test_peer(80000.0, 800.0, false, true)); // Already unchoked, med-high speed
        algo.add_peer(create_test_peer(60000.0, 600.0, true, true)); // Choked, medium speed
        algo.add_peer(create_test_peer(40000.0, 400.0, true, true)); // Choked, lower speed

        // First rotation: top 2 should stay unchoked (they're already there)
        let actions = algo.rotate_choke();

        // Count NoChange actions for the already-unchoked peers
        let no_change_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::NoChange(_)))
            .count();

        // At least the top 2 should have NoChange (they were already unchoked and remain so)
        assert!(
            no_change_count >= 2,
            "Expected at least 2 NoChange actions, got {}",
            no_change_count
        );

        // Second rotation without changes: should produce mostly NoChange
        let actions2 = algo.rotate_choke();
        let no_change_count2 = actions2
            .iter()
            .filter(|a| matches!(a, ChokeAction::NoChange(_)))
            .count();

        // All should be NoChange on second call (idempotent-safe)
        assert_eq!(no_change_count2, 4, "Expected all NoChange on second call");
    }

    #[test]
    fn test_optimistically_unchoke_selects_choked_peer() {
        let config = ChokingConfig {
            optimistic_unchoke_interval_secs: 0, // Allow immediate optimistic unchoke for testing
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add peers
        algo.add_peer(create_test_peer(1000.0, 100.0, true, true)); // Choked + interested ✓
        algo.add_peer(create_test_peer(2000.0, 200.0, false, true)); // Unchoked ✗
        algo.add_peer(create_test_peer(3000.0, 300.0, true, false)); // Not interested ✗

        let result = algo.optimistically_unchoke();

        // Should select peer 0 (only one that meets criteria)
        assert!(
            result.is_some(),
            "Expected to select a peer for optimistic unchoke"
        );
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_optimistically_avoids_recent() {
        let config = ChokingConfig {
            optimistic_unchoke_interval_secs: 30,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add peer and mark it as recently optimistically unchoked
        let mut peer = create_test_peer(1000.0, 100.0, true, true);
        peer.record_optimistic_unchoke(); // Just marked, so < 30s ago
        algo.add_peer(peer);

        let result = algo.optimistically_unchoke();

        // Should not select this peer (too recent)
        assert!(result.is_none());
    }

    #[test]
    fn test_snubbed_peers_get_lowered_score() {
        // Create two identical peers except one is snubbed
        let normal_peer = create_test_peer(50000.0, 500.0, true, true);
        let mut snubbed_peer = create_test_peer(50000.0, 500.0, true, true);
        snubbed_peer.is_snubbed = true;

        let normal_score = ChokingAlgorithm::calculate_peer_score(&normal_peer, false);
        let snubbed_score_stats = ChokingAlgorithm::calculate_peer_score(&snubbed_peer, false);
        let snubbed_score_explicit = ChokingAlgorithm::calculate_peer_score(&normal_peer, true);

        // Snubbed peer should have much lower score (penalty of -1000)
        assert!(snubbed_score_stats < normal_score);
        assert!(
            (normal_score - snubbed_score_stats) > 900.0,
            "Expected large score difference due to PeerStats snubbed penalty"
        );

        // Explicitly snubbed peer should also have much lower score
        assert!(snubbed_score_explicit < normal_score);
        assert!(
            (normal_score - snubbed_score_explicit) > 900.0,
            "Expected large score difference due to explicit snubbed penalty"
        );
    }

    #[test]
    fn test_check_snubbed_returns_timed_out_peers() {
        let config = ChokingConfig {
            snubbed_timeout_secs: 1, // Use 1 second for testing
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Create a peer that hasn't received data
        let _addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let peer = PeerStats::new([0u8; 20], "127.0.0.1:6882".parse().unwrap());
        algo.add_peer(peer);

        // Wait for timeout (slightly longer than snubbed_timeout_secs)
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Check snubbed status
        let snubbed_indices = algo.check_snubbed_peers();

        // Should flag peer 0 as snubbed
        assert_eq!(snubbed_indices.len(), 1);
        assert_eq!(snubbed_indices[0], 0);
        assert!(algo.get_peer(0).unwrap().is_snubbed);
    }

    #[test]
    fn test_on_data_received_resets_snubbed_status() {
        let config = ChokingConfig {
            snubbed_timeout_secs: 1,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let peer = PeerStats::new([0u8; 20], "127.0.0.1:6883".parse().unwrap());
        algo.add_peer(peer);

        // Wait for timeout
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Mark as snubbed
        let snubbed = algo.check_snubbed_peers();
        assert_eq!(snubbed.len(), 1);
        assert!(algo.get_peer(0).unwrap().is_snubbed);

        // Now receive data
        algo.on_data_received(0, 1024);

        // Snubbed status should be reset
        assert!(!algo.get_peer(0).unwrap().is_snubbed);
    }

    #[test]
    fn test_get_peer_accessors() {
        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let peer = PeerStats::new([0u8; 20], addr);
        algo.add_peer(peer);

        // Test immutable access
        assert!(algo.get_peer(0).is_some());
        assert!(algo.get_peer(1).is_none());

        // Test mutable access
        {
            let p = algo.get_peer_mut(0).unwrap();
            p.download_speed = 9999.0;
        }

        assert!((algo.get_peer(0).unwrap().download_speed - 9999.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_defaults() {
        let config = ChokingConfig::default();

        assert_eq!(config.max_upload_slots, 4);
        assert_eq!(config.optimistic_unchoke_interval_secs, 30);
        assert_eq!(config.snubbed_timeout_secs, 60);
        assert_eq!(config.choke_rotation_interval_secs, 10);
    }

    // ==================== G1: Snubbing Enhancement Tests ====================

    #[test]
    fn test_snub_detection_after_timeout() {
        // Test that peers are detected as snubbed after timeout
        let config = ChokingConfig {
            snubbed_timeout_secs: 1,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let peer = PeerStats::new([0u8; 20], "127.0.0.1:6882".parse().unwrap());
        algo.add_peer(peer);

        // Wait for timeout
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Check snubbed status - should detect via PeerStats timeout
        let snubbed_indices = algo.check_snubbed_peers();
        assert_eq!(snubbed_indices.len(), 1);
        assert!(algo.get_peer(0).unwrap().is_snubbed);
    }

    #[test]
    fn test_snubbed_peer_always_choked() {
        // Test that explicitly snubbed peers always get choked
        let config = ChokingConfig {
            max_upload_slots: 2,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add 3 peers - peer 0 is high speed but will be snubbed
        algo.add_peer(create_test_peer(100000.0, 1000.0, true, true)); // Peer 0: high speed
        algo.add_peer(create_test_peer(50000.0, 500.0, true, true)); // Peer 1: medium speed
        algo.add_peer(create_test_peer(30000.0, 300.0, true, true)); // Peer 2: low speed

        // Explicitly snub peer 0 (the highest speed one)
        algo.mark_peer_snubbed(0);
        assert!(algo.is_explicitly_snubbed(0));
        assert_eq!(algo.snubbed_count(), 1);

        // Run choke rotation - snubbed peer should be choked despite high score
        let actions = algo.rotate_choke();

        // Find action for peer 0 - it should be Choked or NoChange(if already choked)
        let peer0_action = actions
            .iter()
            .find(|a| matches!(a, ChokeAction::NoChange(0) | ChokeAction::Choke(0)));
        assert!(
            peer0_action.is_some(),
            "Peer 0 should have an action in results"
        );
        // Peer 0 started as choked (am_choking=true), so with -1000 score it stays choked
        match peer0_action.unwrap() {
            ChokeAction::Choke(_) | ChokeAction::NoChange(_) => {} // Expected
            ChokeAction::Unchoke(_) => panic!("Snubbed peer 0 should NEVER be unchoked"),
        }
    }

    #[test]
    fn test_unsnub_on_data_received() {
        // Test that receiving data from a peer auto-unsnubs them
        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        let peer = PeerStats::new([0u8; 20], "127.0.0.1:6883".parse().unwrap());
        algo.add_peer(peer);

        // Explicitly snub peer 0
        algo.mark_peer_snubbed(0);
        assert!(algo.is_explicitly_snubbed(0));
        assert_eq!(algo.snubbed_count(), 1);

        // Receive data from peer 0 - should auto-unsnub
        algo.on_data_received(0, 1024);
        assert!(
            !algo.is_explicitly_snubbed(0),
            "Peer should be un-snubbed after data received"
        );
        assert_eq!(algo.snubbed_count(), 0);
    }

    #[test]
    fn test_opt_unchoking_rotation_changes_peer() {
        // Test that optimistic unchoke rotates among eligible peers
        let config = ChokingConfig {
            optimistic_unchoke_interval_secs: 0, // Allow immediate re-selection
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Add 3 eligible peers (all choked + interested)
        algo.add_peer(create_test_peer(1000.0, 100.0, true, true));
        algo.add_peer(create_test_peer(2000.0, 200.0, true, true));
        algo.add_peer(create_test_peer(3000.0, 300.0, true, true));

        // First optimistic unchoke
        let first = algo.optimistically_unchoke();
        assert!(first.is_some());
        let first_idx = first.unwrap();

        // Second optimistic unchoke - should pick a DIFFERENT peer (round-robin)
        // Reset the last_optimistic_unchoke time so they're eligible again
        for i in 0..3 {
            if let Some(p) = algo.get_peer_mut(i) {
                p.last_optimistic_unchoke_at =
                    std::time::Instant::now() - std::time::Duration::from_secs(1);
            }
        }

        let second = algo.optimistically_unchoke();
        assert!(second.is_some());
        let second_idx = second.unwrap();

        // With round-robin, second should differ from first (unless only 1 candidate)
        // Since all 3 are eligible and we use rotation, we expect different peer
        assert_ne!(
            first_idx, second_idx,
            "Optimistic unchoke should rotate to a different peer"
        );
    }

    #[test]
    fn test_opt_unchoking_excludes_snubbed_peers() {
        // Test that snubbed peers are excluded from optimistic unchoke candidates
        let config = ChokingConfig {
            optimistic_unchoke_interval_secs: 0,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Peer 0: eligible but will be snubbed
        algo.add_peer(create_test_peer(5000.0, 500.0, true, true));
        // Peer 1: eligible and NOT snubbed
        algo.add_peer(create_test_peer(3000.0, 300.0, true, true));

        // Snub peer 0
        algo.mark_peer_snubbed(0);

        // Optimistic unchoke should ONLY select peer 1
        let result = algo.optimistically_unchoke();
        assert!(result.is_some());
        assert_eq!(
            result.unwrap(),
            1,
            "Should select non-snubbed peer for optimistic unchoke"
        );
    }

    #[test]
    fn test_mark_snubbed_idempotent() {
        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        algo.add_peer(create_test_peer(100.0, 10.0, true, true));

        // Marking same peer twice should not increase count
        algo.mark_peer_snubbed(0);
        assert_eq!(algo.snubbed_count(), 1);
        algo.mark_peer_snubbed(0); // Duplicate
        assert_eq!(
            algo.snubbed_count(),
            1,
            "Duplicate mark should not increase count"
        );
    }

    #[test]
    fn test_unsnub_non_snubbed_peer_returns_false() {
        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        algo.add_peer(create_test_peer(100.0, 10.0, true, true));

        // Unsnubbing a peer that was never snubbed returns false
        let result = algo.unsnub_peer(0);
        assert!(!result, "Unsnubbing non-snubbed peer should return false");
    }
}
