use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::engine::bittorrent::peer::choke_manager::BtSeederStateChoke;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::upload_session::{BtSeedingConfig, PieceDataProvider};
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;

use super::{
    BtSeedManager, CHOKE_ROUND_INTERVAL_SECS, PeerSwarm, SeedExitCondition, SeedPeerDiscovery,
};

impl BtSeedManager {
    /// Create a new seed manager with basic parameters.
    ///
    /// This is the simplest constructor, used by tests and simple seeding setups.
    pub fn new(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::build(
            [0u8; 20],
            connections.into_iter().map(wrap_peer_connection).collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
            PeerSwarm::new(64),
        )
    }

    /// Create a new seed manager with an info hash.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_info_hash(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::build(
            info_hash,
            connections.into_iter().map(wrap_peer_connection).collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
            PeerSwarm::new(64),
        )
    }

    /// Create a seed manager with a tracker announcer for periodic
    /// re-announce while seeding (C++ SeedCheckCommand keeps the swarm
    /// informed of the seeder's continued presence).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_announcer(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
    ) -> Self {
        Self::build(
            info_hash,
            connections.into_iter().map(wrap_peer_connection).collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            announcer,
            peer_id,
            PeerSwarm::new(64),
        )
    }

    /// Create a seed manager with a cancellation token (for external shutdown).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_cancel_token(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        cancel_token: CancellationToken,
    ) -> Self {
        Self::build(
            info_hash,
            connections.into_iter().map(wrap_peer_connection).collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            cancel_token,
            None,
            None,
            [0u8; 20],
            PeerSwarm::new(64),
        )
    }

    /// Build a test seeding manager with pre-established transport variants.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_transports(
        info_hash: [u8; 20],
        connections: Vec<BtPeerConn>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
        incoming_peers: Option<
            tokio::sync::mpsc::Receiver<crate::engine::bittorrent::peer::listener::IncomingPeer>,
        >,
    ) -> Self {
        Self::build(
            info_hash,
            connections,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            incoming_peers.map(|receiver| std::sync::Arc::new(tokio::sync::Mutex::new(receiver))),
            announcer,
            peer_id,
            PeerSwarm::new(64),
        )
    }

    /// Continue a torrent lifecycle with the actor registry already owned by
    /// its download session instead of creating a second peer registry.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_swarm(
        info_hash: [u8; 20],
        swarm: PeerSwarm,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        tracker_actor: Option<
            crate::engine::bittorrent::download::execute::BtTrackerAnnouncerActor,
        >,
        peer_id: [u8; 20],
        incoming_peers: Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
        upload_counter: Arc<AtomicU64>,
    ) -> Self {
        let mut manager = Self::build(
            info_hash,
            Vec::new(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            incoming_peers,
            None,
            peer_id,
            swarm,
        );
        manager.tracker_actor = tracker_actor;
        manager.upload_counter = upload_counter;
        manager
    }

    /// Common builder used by all public constructors.
    #[allow(clippy::too_many_arguments)]
    fn build(
        info_hash: [u8; 20],
        mut pending_connections: Vec<BtPeerConn>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        cancel_token: CancellationToken,
        incoming_peers: Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
        swarm: PeerSwarm,
    ) -> Self {
        let torrent_upload_limiter = RateLimiter::new(&RateLimiterConfig::new(
            None,
            config.max_upload_bytes_per_sec,
        ));
        for connection in &mut pending_connections {
            connection.configure_upload_with_auto_unchoke(
                &config,
                torrent_upload_limiter.clone(),
                piece_provider.num_pieces(),
                piece_provider.piece_length(),
                false,
            );
            connection.stats.am_choking = true;
        }

        let seeder_choke = BtSeederStateChoke::with_slots(config.max_peers_to_unchoke);
        let upload_counter = Arc::new(AtomicU64::new(0));
        for connection in &mut pending_connections {
            connection.set_upload_counter(Arc::clone(&upload_counter));
        }

        Self {
            info_hash,
            pending_connections,
            swarm,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            upload_counter,
            torrent_upload_limiter,
            seeding_start_time: Instant::now(),
            is_active: true,
            seeder_choke,
            cancel_token,
            peer_storage: None,
            peer_discovery: None,
            pending_peer_connection: None,
            halt_requested: false,
            // Run the first choke round immediately after the first peer state
            // update.  A newly admitted interested peer should not wait for
            // the full rotation interval before receiving an unchoke.
            last_choke_time: Instant::now() - Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS),
            announcer: announcer.map(|announcer| Arc::new(tokio::sync::Mutex::new(announcer))),
            tracker_actor: None,
            pending_tracker_announce: None,
            peer_id,
            incoming_peers,
            upload_progress: None,
            connection_state: None,
            peer_snapshot_store: None,
        }
    }

    /// Attach the session-scoped peer storage used to release seeding peers.
    pub fn with_peer_storage(
        mut self,
        peer_storage: std::sync::Arc<
            std::sync::Mutex<crate::engine::bittorrent::peer::storage::DefaultPeerStorage>,
        >,
    ) -> Self {
        self.peer_storage = Some(peer_storage);
        self
    }

    pub(crate) fn with_torrent_upload_limiter(mut self, limiter: RateLimiter) -> Self {
        self.torrent_upload_limiter = limiter;
        self
    }

    pub(crate) fn with_peer_discovery(mut self, discovery: SeedPeerDiscovery) -> Self {
        self.peer_discovery = Some(discovery);
        self
    }
}

fn wrap_peer_connection(
    connection: aria2_protocol::bittorrent::peer::connection::PeerConnection,
) -> BtPeerConn {
    let endpoint = connection
        .remote_addr()
        .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    BtPeerConn::from_incoming_plain(connection, endpoint)
}
