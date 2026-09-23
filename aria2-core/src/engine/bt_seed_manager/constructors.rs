use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::engine::bt_choke_manager::BtSeederStateChoke;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::bt_upload_session::{BtSeedingConfig, PieceDataProvider};
use crate::engine::peer_stats::PeerStats;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS, SeedExitCondition};

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
            connections
                .into_iter()
                .map(|connection| {
                    configure_upload_peer(connection, &config, piece_provider.as_ref())
                })
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
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
            connections
                .into_iter()
                .map(|connection| {
                    configure_upload_peer(connection, &config, piece_provider.as_ref())
                })
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
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
            connections
                .into_iter()
                .map(|connection| {
                    configure_upload_peer(connection, &config, piece_provider.as_ref())
                })
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            CancellationToken::new(),
            None,
            announcer,
            peer_id,
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
            connections
                .into_iter()
                .map(|connection| {
                    configure_upload_peer(connection, &config, piece_provider.as_ref())
                })
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            cancel_token,
            None,
            None,
            [0u8; 20],
        )
    }

    /// Construct the production seeding manager with the transport variants
    /// already accepted by the download loop and its live incoming-peer route.
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
            tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>,
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
            incoming_peers,
            announcer,
            peer_id,
        )
    }

    /// Common builder used by all public constructors.
    #[allow(clippy::too_many_arguments)]
    fn build(
        info_hash: [u8; 20],
        mut connections: Vec<BtPeerConn>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        cancel_token: CancellationToken,
        incoming_peers: Option<
            tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>,
        >,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
    ) -> Self {
        for connection in &mut connections {
            connection.configure_upload_with_auto_unchoke(
                &config,
                piece_provider.num_pieces(),
                piece_provider.piece_length(),
                false,
            );
            connection.stats.am_choking = true;
        }

        // Initialise PeerStats for each session (the seeder-state algorithm
        // needs peer_interested, upload_speed, etc.). Keep the transport
        // endpoint as the identity used by the choking and reporting layers.
        let peer_stats: Vec<PeerStats> = connections
            .iter()
            .map(|connection| connection.stats.clone())
            .collect();

        let seeder_choke = BtSeederStateChoke::with_slots(config.max_peers_to_unchoke);
        let upload_counter = Arc::new(AtomicU64::new(0));
        for connection in &mut connections {
            connection.set_upload_counter(Arc::clone(&upload_counter));
        }

        Self {
            info_hash,
            upload_sessions: connections,
            seed_peer_actors: Vec::new(),
            seed_peer_actor_indices: HashMap::new(),
            seed_peer_event_tx: None,
            seed_peer_event_rx: None,
            peer_stats,
            piece_provider: Some(piece_provider),
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            upload_counter,
            seeding_start_time: Instant::now(),
            is_active: true,
            seeder_choke,
            cancel_token,
            peer_storage: None,
            halt_requested: false,
            // Run the first choke round immediately after the first peer state
            // update.  A newly admitted interested peer should not wait for
            // the full rotation interval before receiving an unchoke.
            last_choke_time: Instant::now() - Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS),
            announcer,
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
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
    ) -> Self {
        self.peer_storage = Some(peer_storage);
        self
    }
}

fn configure_upload_peer(
    connection: aria2_protocol::bittorrent::peer::connection::PeerConnection,
    config: &BtSeedingConfig,
    provider: &dyn PieceDataProvider,
) -> BtPeerConn {
    let endpoint = connection
        .remote_addr()
        .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    let mut peer = BtPeerConn::from_incoming_plain(connection, endpoint);
    peer.configure_upload_with_auto_unchoke(
        config,
        provider.num_pieces(),
        provider.piece_length(),
        false,
    );
    peer.stats.am_choking = true;
    peer
}
