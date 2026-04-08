use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, debug, warn};

use crate::error::Result;

use super::bt_upload_session::{
    BtUploadSession, BtSeedingConfig, PieceDataProvider,
};

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
        Self { seed_time: None, seed_ratio: None }
    }

    pub fn with_time(secs: u64) -> Self {
        if secs == 0 {
            Self::infinite()
        } else {
            Self { seed_time: Some(Duration::from_secs(secs)), seed_ratio: None }
        }
    }

    pub fn with_ratio(ratio: f64) -> Self {
        if ratio <= 0.0 {
            Self::infinite()
        } else {
            Self { seed_time: None, seed_ratio: Some(ratio) }
        }
    }

    pub fn with_time_and_ratio(secs: u64, ratio: f64) -> Self {
        let time = if secs == 0 { None } else { Some(Duration::from_secs(secs)) };
        let r = if ratio <= 0.0 { None } else { Some(ratio) };
        Self { seed_time: time, seed_ratio: r }
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
}

impl BtSeedManager {
    pub fn new(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
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
        }
    }

    pub async fn run_seeding_loop(&mut self) -> Result<()> {
        info!("Seeding started: {} peers, condition={:?}",
            self.sessions.len(), self.exit_condition);

        for session in &mut self.sessions {
            session.unchoke_peer().await.ok();
        }

        loop {
            let mut alive_sessions = Vec::new();
            for session in &mut self.sessions {
                if !session.is_dead() {
                    match session.handle_incoming_messages(self.piece_data.as_ref()).await {
                        Ok(uploaded) => { self.total_uploaded += uploaded; }
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

            self.maybe_optimistic_unchoke().await;

            if self.should_exit() {
                info!("Seeding exit condition met after {:?}", self.seeding_start_time.elapsed());
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

        let choked_indices: Vec<usize> = self.sessions.iter()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bt_upload_session::InMemoryPieceProvider;

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
        let manager = make_test_manager(
            SeedExitCondition::with_time(1),
            1000,
            500,
        );
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(2);
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_exit_by_ratio() {
        let manager = make_test_manager(
            SeedExitCondition::with_ratio(1.0),
            1000,
            499,
        );
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 1500;
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_not_exit_early() {
        let manager = make_test_manager(
            SeedExitCondition::with_time_and_ratio(10, 3.0),
            1000,
            100,
        );
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 2000;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(5);
        assert!(!manager.should_exit(), "Neither time nor ratio reached yet");
    }

    #[test]
    fn test_seed_manager_stats() {
        let manager = make_test_manager(
            SeedExitCondition::infinite(),
            1024 * 100,
            51200,
        );
        assert_eq!(manager.num_total_peers(), 0);
        assert_eq!(manager.num_alive_peers(), 0);
        assert_eq!(manager.total_uploaded(), 51200);
    }

    fn make_test_manager(exit_cond: SeedExitCondition, downloaded: u64, uploaded: u64) -> BtSeedManager {
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        let mut mgr = BtSeedManager::new(conns, provider, config, exit_cond, downloaded);
        mgr.total_uploaded = uploaded;
        mgr
    }
}
