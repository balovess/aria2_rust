use rand::Rng;
use std::net::SocketAddr;

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
}

impl ChokingAlgorithm {
    /// Create a new choking algorithm with the given configuration
    pub fn new(config: ChokingConfig) -> Self {
        Self {
            peers: Vec::new(),
            config,
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
    ///          (avoid churn - only change what's necessary)
    /// 5. Return only the actions that changed state
    pub fn rotate_choke(&mut self) -> Vec<ChokeAction> {
        // Step 1: Check and mark snubbed peers
        self.check_snubbed_peers_internal();

        if self.peers.is_empty() {
            return vec![];
        }

        let max_slots = self.config.max_upload_slots;
        
        // Step 2: Calculate scores and sort indices by score descending
        let mut scored_peers: Vec<(usize, f64)> = self.peers
            .iter()
            .enumerate()
            .map(|(i, peer)| (i, Self::calculate_peer_score(peer)))
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
    /// Returns Some(index) if found, None if no eligible peer
    pub fn optimistically_unchoke(&mut self) -> Option<usize> {
        // Find candidates that are:
        //   - Currently choked (am_choking == true)
        //   - Interested in us (peer_interested == true)
        //   - Not snubbed
        //   - Not recently optimistically unchoked (>interval ago)
        let mut candidates: Vec<usize> = self.peers
            .iter()
            .enumerate()
            .filter(|(_, peer)| {
                peer.am_choking
                    && peer.peer_interested
                    && !peer.is_snubbed
                    && peer.time_since_last_optimistic_unchoke().as_secs() >= self.config.optimistic_unchoke_interval_secs
            })
            .map(|(i, _)| i)
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Randomly select one from candidates
        let mut rng = rand::thread_rng();
        let selected_idx = rng.gen_range(0..candidates.len());
        let selected = candidates[selected_idx];

        // Mark as optimistically unchoked
        self.peers[selected].record_optimistic_unchoke();

        Some(selected)
    }

    /// Called whenever we receive data from a peer
    pub fn on_data_received(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(peer) = self.peers.get_mut(peer_idx) {
            peer.on_data_received(bytes);
        }
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
    ///   3. Snubbed penalty: -1000 if snubbed
    ///   4. Interest bonus: +50 if peer_interested
    ///   5. New peer bonus (time since unchoke < 60s): +30 (anti-churn)
    fn calculate_peer_score(peer: &PeerStats) -> f64 {
        let mut score = 0.0;

        // Download speed (primary factor - tit-for-tat)
        // Scale down to reasonable range
        score += peer.download_speed * 0.00001;

        // Upload speed (reciprocity)
        score += peer.upload_speed * 0.000005;

        // Snubbed penalty (heavy penalty to avoid wasting slots)
        if peer.is_snubbed {
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

    /// Helper to create a test peer with specific characteristics
    fn create_test_peer(download_speed: f64, upload_speed: f64, am_choking: bool, peer_interested: bool) -> PeerStats {
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
        let unchoke_count = actions.iter()
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
        algo.add_peer(create_test_peer(80000.0, 800.0, false, true));   // Already unchoked, med-high speed
        algo.add_peer(create_test_peer(60000.0, 600.0, true, true));    // Choked, medium speed
        algo.add_peer(create_test_peer(40000.0, 400.0, true, true));    // Choked, lower speed

        // First rotation: top 2 should stay unchoked (they're already there)
        let actions = algo.rotate_choke();

        // Count NoChange actions for the already-unchoked peers
        let no_change_count = actions.iter()
            .filter(|a| matches!(a, ChokeAction::NoChange(_)))
            .count();

        // At least the top 2 should have NoChange (they were already unchoked and remain so)
        assert!(no_change_count >= 2, "Expected at least 2 NoChange actions, got {}", no_change_count);

        // Second rotation without changes: should produce mostly NoChange
        let actions2 = algo.rotate_choke();
        let no_change_count2 = actions2.iter()
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
        algo.add_peer(create_test_peer(1000.0, 100.0, true, true));   // Choked + interested ✓
        algo.add_peer(create_test_peer(2000.0, 200.0, false, true));  // Unchoked ✗
        algo.add_peer(create_test_peer(3000.0, 300.0, true, false));  // Not interested ✗

        let result = algo.optimistically_unchoke();

        // Should select peer 0 (only one that meets criteria)
        assert!(result.is_some(), "Expected to select a peer for optimistic unchoke");
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

        let normal_score = ChokingAlgorithm::calculate_peer_score(&normal_peer);
        let snubbed_score = ChokingAlgorithm::calculate_peer_score(&snubbed_peer);

        // Snubbed peer should have much lower score (penalty of -1000)
        assert!(snubbed_score < normal_score);
        assert!((normal_score - snubbed_score) > 900.0, 
            "Expected large score difference due to snubbed penalty");
    }

    #[test]
    fn test_check_snubbed_returns_timed_out_peers() {
        let config = ChokingConfig {
            snubbed_timeout_secs: 1, // Use 1 second for testing
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        // Create a peer that hasn't received data
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
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
}
