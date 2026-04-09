use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::error::Result;

use super::bt_upload_session::{BtSeedingConfig, BtUploadSession, PieceDataProvider};
use super::choking_algorithm::{ChokingAlgorithm, ChokeAction};

#[derive(Debug, Clone)]
pub struct SeedExitCondition {
    pub seed_time: Option<Duration>,
    pub seed_ratio: Option<f64>,
}

impl Default for SeedExitCondition {
    fn default() -> Self {
        Self {
            seed_time: None,
            seed_ratio: None,
        }
    }
}

impl SeedExitCondition {
    pub fn infinite() -> Self {
        Self {
            seed_time: None,
            seed_ratio: None,
        }
    }

    pub fn with_time(secs: u64) -> Self {
        if secs == 0 {
            Self::infinite()
        } else {
            Self {
                seed_time: Some(Duration::from_secs(secs)),
                seed_ratio: None,
            }
        }
    }

    pub fn with_ratio(ratio: f64) -> Self {
        if ratio <= 0.0 {
            Self::infinite()
        } else {
            Self {
                seed_time: None,
                seed_ratio: Some(ratio),
            }
        }
    }

    pub fn with_time_and_ratio(secs: u64, ratio: f64) -> Self {
        let time = if secs == 0 {
            None
        } else {
            Some(Duration::from_secs(secs))
        };
        let r = if ratio <= 0.0 { None } else { Some(ratio) };
        Self {
            seed_time: time,
            seed_ratio: r,
        }
    }
}

pub struct BtSeedManager {
    sessions: Vec<BtUploadSession>,
    piece_data: Arc<dyn PieceDataProvider>,
    config: BtSeedingConfig,
    exit_condition: SeedExitCondition,
    pub total_uploaded: u64,
    total_downloaded: u64,
    pub seeding_start_time: Instant,
    last_optimistic_unchoke: Instant,
    optimistic_round: usize,
    /// Choking algorithm for tit-for-tat peer selection during seeding.
    /// When present, drives intelligent choke/unchoke decisions every rotation interval.
    pub choking_algo: Option<ChokingAlgorithm>,
}

impl BtSeedManager {
    pub fn new(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::new_with_choking_algo(connections, piece_data, config, exit_condition, total_downloaded, None)
    }

    /// Create a new BtSeedManager with an optional ChokingAlgorithm.
    ///
    /// When `choking_algo` is `Some`, the seeding loop will call
    /// [`ChokingAlgorithm::rotate_choke`] every `config.choke_rotation_interval_secs`
    /// and apply the resulting choke/unchoke actions to sessions.
    pub fn new_with_choking_algo(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
    ) -> Self {
        let sessions = connections
            .into_iter()
            .map(|conn| BtUploadSession::new(conn, &config))
            .collect();

        Self {
            sessions,
            piece_data,
            config,
            exit_condition,
            total_uploaded: 0,
            total_downloaded,
            seeding_start_time: Instant::now(),
            last_optimistic_unchoke: Instant::now(),
            optimistic_round: 0,
            choking_algo,
        }
    }

    pub async fn run_seeding_loop(&mut self) -> Result<()> {
        info!(
            "Seeding started: {} peers, condition={:?}, choking_algo={}",
            self.sessions.len(),
            self.exit_condition,
            self.choking_algo.is_some()
        );

        for session in &mut self.sessions {
            session.unchoke_peer().await.ok();
        }

        // Determine choke rotation interval from choking_algo config or fallback
        let choke_rotation_secs = self
            .choking_algo
            .as_ref()
            .map(|c| c.config().choke_rotation_interval_secs)
            .unwrap_or(10);
        let mut choke_interval = tokio::time::interval(Duration::from_secs(choke_rotation_secs));
        // Don't let the interval accumulate missed ticks
        choke_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // --- Choking algorithm rotation (every N seconds) ---
            if let Some(ref mut algo) = self.choking_algo {
                choke_interval.tick().await;
                let actions = algo.rotate_choke();
                for action in actions {
                    match action {
                        ChokeAction::Unchoke(idx) => {
                            if let Some(session) = self.sessions.get_mut(idx) {
                                if !session.is_dead() && session.is_peer_choked() {
                                    debug!("ChokingAlgo: Unchoke peer #{}", idx);
                                    session.unchoke_peer().await.ok();
                                }
                            }
                        }
                        ChokeAction::Choke(idx) => {
                            if let Some(session) = self.sessions.get_mut(idx) {
                                if !session.is_dead() && !session.is_peer_choked() {
                                    debug!("ChokingAlgo: Choke peer #{}", idx);
                                    session.choke_peer().await.ok();
                                }
                            }
                        }
                        ChokeAction::NoChange(_) => {}
                    }
                }
            }

            // --- Process incoming messages from all sessions ---
            let mut alive_sessions = Vec::new();
            for session in &mut self.sessions {
                if !session.is_dead() {
                    match session
                        .handle_incoming_messages(self.piece_data.as_ref())
                        .await
                    {
                        Ok(uploaded) => {
                            self.total_uploaded += uploaded;
                        }
                        Err(e) => {
                            warn!("Upload session error: {}", e);
                            session.is_dead = true;
                        }
                    }
                }
                if !session.is_dead() {
                    alive_sessions.push(session.uploaded_bytes());
                }
            }

            // Fallback: optimistic unchoke when no choking algorithm is configured
            if self.choking_algo.is_none() {
                self.maybe_optimistic_unchoke().await;
            }

            if self.should_exit() {
                info!(
                    "Seeding exit condition met after {:?}",
                    self.seeding_start_time.elapsed()
                );
                break;
            }

            if alive_sessions.is_empty() && !self.sessions.is_empty() {
                debug!("All upload peers disconnected");
                break;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        for session in &mut self.sessions {
            if !session.is_dead() {
                session.choke_peer().await.ok();
            }
        }

        Ok(())
    }

    pub fn should_exit(&self) -> bool {
        let elapsed = self.seeding_start_time.elapsed();

        if let Some(max_time) = self.exit_condition.seed_time {
            if elapsed >= max_time {
                return true;
            }
        }

        if let Some(ratio) = self.exit_condition.seed_ratio {
            if self.total_downloaded > 0 {
                let actual_ratio = self.total_uploaded as f64 / self.total_downloaded as f64;
                if actual_ratio >= ratio {
                    return true;
                }
            }
        }

        false
    }

    async fn maybe_optimistic_unchoke(&mut self) {
        let interval = Duration::from_secs(self.config.optimistic_unchoke_interval_secs);
        if self.last_optimistic_unchoke.elapsed() < interval {
            return;
        }
        self.last_optimistic_unchoke = Instant::now();
        self.optimistic_round += 1;

        let choked_indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_dead() && s.is_peer_choked())
            .map(|(i, _)| i)
            .collect();

        if choked_indices.is_empty() {
            return;
        }

        let idx = self.optimistic_round % choked_indices.len();
        let target = choked_indices[idx];
        if let Some(session) = self.sessions.get_mut(target) {
            debug!("Optimistic unchoke peer #{}", target);
            session.unchoke_peer().await.ok();
        }
    }

    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded
    }

    pub fn seeding_duration(&self) -> Duration {
        self.seeding_start_time.elapsed()
    }

    pub fn num_alive_peers(&self) -> usize {
        self.sessions.iter().filter(|s| !s.is_dead()).count()
    }

    pub fn num_total_peers(&self) -> usize {
        self.sessions.len()
    }

    // ------------------------------------------------------------------
    // Choking algorithm integration helpers
    // ------------------------------------------------------------------

    /// Sync upload statistics from sessions into the choking algorithm.
    ///
    /// Call this periodically (e.g., after each message handling round) so
    /// the algorithm has up-to-date speed data for scoring.
    pub fn sync_choking_algo_stats(&mut self) {
        if let Some(ref mut algo) = self.choking_algo {
            for (i, session) in self.sessions.iter().enumerate() {
                if let Some(peer) = algo.get_peer_mut(i) {
                    // Update uploaded bytes from the session
                    let session_uploaded = session.uploaded_bytes();
                    if session_uploaded > peer.uploaded_bytes {
                        peer.on_data_sent(session_uploaded - peer.uploaded_bytes);
                    }
                }
            }
        }
    }

    /// Get a reference to the choking algorithm, if configured.
    pub fn choking_algo(&self) -> Option<&ChokingAlgorithm> {
        self.choking_algo.as_ref()
    }

    /// Get a mutable reference to the choking algorithm, if configured.
    pub fn choking_algo_mut(&mut self) -> Option<&mut ChokingAlgorithm> {
        self.choking_algo.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bt_upload_session::InMemoryPieceProvider;
    use crate::engine::choking_algorithm::{ChokingConfig, ChokeAction};
    use crate::engine::peer_stats::PeerStats;
    use std::net::SocketAddr;

    #[test]
    fn test_exit_condition_default_infinite() {
        let cond = SeedExitCondition::default();
        assert!(cond.seed_time.is_none());
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_exit_condition_with_time_zero_is_infinite() {
        let cond = SeedExitCondition::with_time(0);
        assert!(cond.seed_time.is_none());
    }

    #[test]
    fn test_exit_condition_with_time_positive() {
        let cond = SeedExitCondition::with_time(60);
        assert_eq!(cond.seed_time, Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_exit_condition_with_ratio_zero_is_infinite() {
        let cond = SeedExitCondition::with_ratio(0.0);
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_exit_condition_with_ratio_positive() {
        let cond = SeedExitCondition::with_ratio(1.5);
        assert_eq!(cond.seed_ratio, Some(1.5));
    }

    #[test]
    fn test_exit_condition_combined() {
        let cond = SeedExitCondition::with_time_and_ratio(120, 2.0);
        assert_eq!(cond.seed_time, Some(Duration::from_secs(120)));
        assert_eq!(cond.seed_ratio, Some(2.0));
    }

    #[test]
    fn test_should_exit_by_time() {
        let manager = make_test_manager(SeedExitCondition::with_time(1), 1000, 500);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(2);
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_exit_by_ratio() {
        let manager = make_test_manager(SeedExitCondition::with_ratio(1.0), 1000, 499);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 1500;
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_not_exit_early() {
        let manager = make_test_manager(SeedExitCondition::with_time_and_ratio(10, 3.0), 1000, 100);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 2000;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(5);
        assert!(!manager.should_exit(), "Neither time nor ratio reached yet");
    }

    #[test]
    fn test_seed_manager_stats() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1024 * 100, 51200);
        assert_eq!(manager.num_total_peers(), 0);
        assert_eq!(manager.num_alive_peers(), 0);
        assert_eq!(manager.total_uploaded(), 51200);
    }

    fn make_test_manager(
        exit_cond: SeedExitCondition,
        downloaded: u64,
        uploaded: u64,
    ) -> BtSeedManager {
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        let mut mgr = BtSeedManager::new(conns, provider, config, exit_cond, downloaded);
        mgr.total_uploaded = uploaded;
        mgr
    }

    fn make_test_manager_with_choking_algo(
        exit_cond: SeedExitCondition,
        downloaded: u64,
        uploaded: u64,
    ) -> BtSeedManager {
        use std::net::SocketAddr;
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        // Create a choking algorithm with fast rotation for testing
        let choking_config = ChokingConfig {
            max_upload_slots: 2,
            optimistic_unchoke_interval_secs: 1,
            snubbed_timeout_secs: 1,
            choke_rotation_interval_secs: 1,
        };
        let mut algo = ChokingAlgorithm::new(choking_config);
        // Add dummy peer stats for testing
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        algo.add_peer(PeerStats::new([0u8; 20], addr));

        let mut mgr = BtSeedManager::new_with_choking_algo(
            conns, provider, config, exit_cond, downloaded, Some(algo),
        );
        mgr.total_uploaded = uploaded;
        mgr
    }

    // ==================================================================
    // Choking algorithm integration tests
    // ==================================================================

    #[test]
    fn test_bt_seed_manager_without_choking_algo_backward_compat() {
        // Verify that BtSeedManager works without choking_algo (backward compatibility)
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 500);
        assert!(manager.choking_algo.is_none());
        assert!(manager.choking_algo().is_none());
        assert!(manager.choking_algo_mut().is_none());

        // Stats should still work
        assert_eq!(manager.num_total_peers(), 0);
        assert_eq!(manager.num_alive_peers(), 0);
        assert_eq!(manager.total_uploaded(), 500);

        // sync_choking_algo_stats should be a no-op when algo is None
        manager.sync_choking_algo_stats(); // Should not panic
    }

    #[test]
    fn test_bt_seed_manager_with_choking_algo() {
        // Verify BtSeedManager with choking_algo initialized correctly
        let manager = make_test_manager_with_choking_algo(SeedExitCondition::infinite(), 2000, 800);
        assert!(manager.choking_algo.is_some());

        // Check algo has peers
        let algo = manager.choking_algo().unwrap();
        assert_eq!(algo.len(), 1);
        assert!(!algo.is_empty());

        // Stats should work
        assert_eq!(manager.num_total_peers(), 0); // sessions are empty (no real connections)
        assert_eq!(manager.total_uploaded(), 800);
    }

    #[test]
    fn test_bt_seed_manager_choking_algo_rotate_choke() {
        // Verify rotate_choke produces actions through the seed manager
        let mut manager = make_test_manager_with_choking_algo(SeedExitCondition::infinite(), 0, 0);

        // Get mutable access and call rotate_choke
        if let Some(algo) = manager.choking_algo_mut() {
            let actions = algo.rotate_choke();

            // With max_upload_slots=2 and 1 peer, we expect:
            // - The peer should be unchoked (it's in top K)
            let unchoke_count = actions.iter()
                .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
                .count();

            assert_eq!(actions.len(), 1, "Should have one action for one peer");
            assert_eq!(unchoke_count, 1, "Single peer should be unchoked (top-K)");
        } else {
            panic!("Expected choking_algo to be present");
        }
    }

    #[test]
    fn test_bt_seed_manager_new_with_none_algo() {
        // new() should produce None for choking_algo (backward compat)
        use std::net::SocketAddr;
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        let mgr = BtSeedManager::new(conns, provider, config, SeedExitCondition::infinite(), 0);
        assert!(mgr.choking_algo.is_none());
    }

    #[test]
    fn test_bt_seed_manager_new_with_some_algo() {
        // new_with_choking_algo(Some(...)) should preserve it
        use std::net::SocketAddr;
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        let choking_config = ChokingConfig::default();
        let algo = ChokingAlgorithm::new(choking_config);

        let mgr = BtSeedManager::new_with_choking_algo(
            conns, provider, config, SeedExitCondition::infinite(), 0, Some(algo),
        );
        assert!(mgr.choking_algo.is_some());
        assert_eq!(mgr.choking_algo.unwrap().len(), 0); // No peers added yet
    }
}
