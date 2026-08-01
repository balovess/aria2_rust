use std::collections::HashMap;
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{
    BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY, PUBLIC_TRACKER_PEER_THRESHOLD,
};
use crate::engine::bt_handshake_validation::filter_duplicate_peer_connections;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::peer_stats::PeerStats;
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    /// Discover peers via tracker announce (HTTP/UDP), DHT, public trackers, and LPD.
    ///
    /// Uses the `TrackerAnnouncer` state machine for proper HTTP/UDP dispatch,
    /// tier rotation, and event management (Started → Downloading → Completed/Stopped).
    pub(super) async fn discover_peers(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        total_size: u64,
        info_hash_raw: &[u8; 20],
    ) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();

        // Initialize the unified TrackerAnnouncer from the torrent's announce list.
        // This replaces the separate HTTP-only + ad-hoc UDP approach with a single
        // state machine that properly routes HTTP vs UDP based on URL scheme.
        // `--bt-tracker` overrides the torrent's own announce list
        // (C++: option value replaces announce URLs).
        let tracker_override: Option<Vec<String>> = {
            let g = self.group.recover();
            g.options().bt_tracker.clone()
        };
        let mut announcer = match tracker_override {
            Some(list) if !list.is_empty() => {
                info!(
                    count = list.len(),
                    "Using user-specified trackers from --bt-tracker"
                );
                let tiers: Vec<Vec<String>> =
                    list.into_iter().map(|u| vec![u]).collect();
                TrackerAnnouncer::new(&tiers, &None)
            }
            _ => TrackerAnnouncer::new(&meta.announce_list, &Some(meta.announce.clone())),
        };

        // Set up UDP client for UDP tracker support
        if let Ok(udp) = UdpTrackerClient::new(0).await {
            let shared = std::sync::Arc::new(tokio::sync::Mutex::new(udp));
            self.udp_client = Some(std::sync::Arc::clone(&shared));
            announcer.set_udp_client(shared);
        }

        let mut peer_addrs: Vec<(String, u16)> = Vec::new();

        // Try tracker announces through the state machine (handles both HTTP and UDP)
        let mut announce_attempts = 0;
        const MAX_ANNOUNCE_ATTEMPTS: usize = 5;

        while announcer.is_announce_ready() && announce_attempts < MAX_ANNOUNCE_ATTEMPTS {
            if let Some(result) = announcer
                .announce(info_hash_raw, &my_peer_id, 0, total_size, 0)
                .await
            {
                debug!(
                    "[BT] Tracker announce result: {} peers from {} (event={:?}, interval={}s, seeders={}, leechers={})",
                    result.peers.len(),
                    result.tracker_url,
                    result.event,
                    result.interval.as_secs(),
                    result.seeders,
                    result.leechers
                );
                peer_addrs.extend(result.peers);

                // If we got peers, no need to try more trackers immediately
                if !peer_addrs.is_empty() {
                    break;
                }
            }
            announce_attempts += 1;
        }

        // Fall back to simple HTTP announce if the state machine didn't produce peers
        if peer_addrs.is_empty() {
            debug!("[BT] State-machine announce produced no peers, trying simple HTTP announce");
            match crate::engine::bt_tracker_comm::perform_http_tracker_announce(
                &meta.announce,
                info_hash_raw,
                &my_peer_id,
                total_size,
            )
            .await
            {
                Ok(peers) => {
                    // perform_http_tracker_announce returns Vec<PeerAddr>
                    for p in peers {
                        peer_addrs.push((p.ip.clone(), p.port));
                    }
                }
                Err(e) => {
                    debug!("[BT] Simple HTTP announce also failed: {}", e);
                }
            }
        }

        // Store the announcer for periodic re-announce during download
        self.tracker_announcer = Some(announcer);

        let mut peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = peer_addrs
            .into_iter()
            .map(|(ip, port)| {
                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port)
            })
            .collect();

        if peer_addrs.is_empty() {
            tracing::error!("[BT] ERROR: No peers from tracker");
        }

        // BEP 0027 (Private Torrent): DHT must be disabled for private torrents
        // to prevent leaking the info_hash to the public DHT network.
        let enable_dht = { self.group.recover().options().enable_dht } && !self.is_private;
        if self.is_private {
            info!("[BT] Private torrent: DHT disabled (BEP 0027)");
        }
        if enable_dht && self.dht_engine.is_none() {
            let dht_port = { self.group.recover().options().dht_listen_port };
            let dht_file_path = { self.group.recover().options().dht_file_path.clone() };
            let dht_entry_points = { self.group.recover().options().dht_entry_point.clone() };

            // Parse custom bootstrap nodes if provided
            let bootstrap_nodes: Vec<std::net::SocketAddr> =
                if let Some(ref entry_points) = dht_entry_points {
                    entry_points
                        .iter()
                        .filter_map(|ep| ep.parse::<std::net::SocketAddr>().ok())
                        .collect()
                } else {
                    vec![]
                };

            let dht_config = aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
                port: dht_port.unwrap_or(0),
                dht_file_path: dht_file_path.map(std::path::PathBuf::from),
                ..Default::default()
            };

            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
                    // Add custom bootstrap nodes to routing table
                    if !bootstrap_nodes.is_empty() {
                        for addr in &bootstrap_nodes {
                            engine.add_node(*addr).await;
                        }
                        tracing::info!(
                            "[BT] Added {} custom DHT bootstrap nodes",
                            bootstrap_nodes.len()
                        );
                    }

                    self.dht_engine = Some(engine);
                    tracing::info!("[BT] DHT engine started");
                    if let Some(dht) = self.dht_engine.as_ref() {
                        dht.start_maintenance_loop();
                    }
                }
                Err(e) => {
                    warn!("[BT] DHT engine start failed: {}", e);
                }
            }
        }

        if let Some(ref engine) = self.dht_engine
            && let Ok(result) = engine.find_peers(info_hash_raw).await {
                if !result.peers.is_empty() {
                    let before = peer_addrs.len();
                    for addr in &result.peers {
                        let ip_str = addr.ip().to_string();
                        let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                            &ip_str,
                            addr.port(),
                        );
                        if !peer_addrs
                            .iter()
                            .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                        {
                            peer_addrs.push(paddr);
                        }
                    }
                    tracing::info!(
                        "[BT] DHT discovered {} extra peers (total: {}, contacted {} DHT nodes)",
                        peer_addrs.len() - before,
                        peer_addrs.len(),
                        result.nodes_contacted
                    );
                } else {
                    debug!("[BT] DHT find_peers returned no peers");
                }
            }

        // BEP 0027 (Private Torrent): public tracker announcement is forbidden
        // for private torrents because it would leak the info_hash to trackers
        // not explicitly listed in the torrent's announce list.
        let enable_public_trackers =
            { self.group.recover().options().enable_public_trackers } && !self.is_private;
        if self.is_private {
            info!("[BT] Private torrent: public trackers disabled (BEP 0027)");
        }
        if enable_public_trackers
            && self.public_trackers.is_none()
            && peer_addrs.len() < PUBLIC_TRACKER_PEER_THRESHOLD
        {
            let ptl = std::sync::Arc::new(
                aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
            );
            ptl.start_auto_update(
                "https://cf.trackerslist.com/best.txt".to_string(),
                std::time::Duration::from_secs(86400),
            );
            self.public_trackers = Some(ptl);
        }

        if let Some(ref pt) = self.public_trackers {
            let http_urls = pt.get_http_trackers().await;
            let mut extra_peers: Vec<(String, u16)> = Vec::new();
            let mut announced = 0usize;

            for url in http_urls.iter().take(MAX_PUBLIC_TRACKERS_TO_TRY) {
                match crate::engine::bt_tracker_comm::announce_to_public_tracker(
                    url,
                    info_hash_raw,
                    &my_peer_id,
                    total_size,
                )
                .await
                {
                    Ok(peers) => {
                        announced += 1;
                        extra_peers.extend(peers);
                    }
                    Err(e) => {
                        debug!("[BT] Public tracker {} failed: {}", url, e);
                    }
                }
            }

            if !extra_peers.is_empty() {
                let before = peer_addrs.len();
                for (ip, port) in extra_peers {
                    let paddr =
                        aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port);
                    if !peer_addrs
                        .iter()
                        .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }
                tracing::info!(
                    "[BT] Public trackers discovered {} extra peers (announced to {} of {})",
                    peer_addrs.len() - before,
                    announced,
                    http_urls.len()
                );
            } else if announced > 0 {
                debug!("[BT] Public trackers responded but no peers found");
            }
        }

        // P2: Integrate LPD-discovered LAN peers
        // BEP 0027 (Private Torrent): LPD (Local Peer Discovery) uses UDP
        // multicast which would leak the info_hash to the local network, so it
        // must be disabled for private torrents.
        if self.is_private {
            if self.lpd_manager.is_some() {
                info!("[BT] Private torrent: LPD disabled (BEP 0027)");
            }
        } else if let Some(ref lpd) = self.lpd_manager {
            // Convert raw 20-byte info_hash to 40-char hex string for LPD
            let info_hash_hex = hex::encode(*info_hash_raw);
            let lpd_peers = lpd.get_peers_for(&info_hash_hex).await;
            if !lpd_peers.is_empty() {
                let before = peer_addrs.len();
                for lpd_peer in &lpd_peers {
                    let ip_str = lpd_peer.addr.to_string();
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                        &ip_str,
                        lpd_peer.port,
                    );
                    if !peer_addrs
                        .iter()
                        .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }

                info!(
                    lpd_count = lpd_peers.len(),
                    total_added = peer_addrs.len() - before,
                    "LPD discovered local peers"
                );

                // Register current download for LPD announcement.
                // Pass private_torrent from TorrentAttribute (BEP 0027):
                // private torrents must NOT be announced via LPD.
                let is_private = self.is_private;
                let _ = lpd.register_torrent(&info_hash_hex, is_private).await;
            } else {
                debug!("LPD no local peers found for this torrent");
            }
        }

        Ok(peer_addrs)
    }

    /// Establish connections to discovered peers and initialize the choking algorithm.
    ///
    /// After establishing connections, this method filters out:
    /// - Self-connections (peer ID matching our own local peer ID)
    /// - Duplicate connections (two connections sharing the same remote peer ID)
    ///
    /// Mirrors C++ `DefaultBtInteractive::receiveHandshake()` which checks:
    /// 1. `memcmp(message->getPeerId(), bittorrent::getStaticPeerId(), 20) == 0`
    ///    → disconnect self-connection
    /// 2. `for(auto& peer : peerStorage_->getUsedPeers()) { memcmp(...) }`
    ///    → disconnect duplicate peer
    pub(super) async fn connect_to_peers(
        &mut self,
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
    ) -> Result<Vec<BtPeerConn>> {
        let require_crypto = { self.group.recover().options().bt_require_crypto };
        let force_encrypt = { self.group.recover().options().bt_force_encrypt };

        // Generate our local peer ID for this session. This is used for
        // self-connection detection (C++ bittorrent::getStaticPeerId()).
        // Note: C++ generates the static peer ID once per session; here we
        // generate it at connection time. For future sessions, this should
        // be a per-session singleton.
        let local_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();

        let conn_result = BtPeerInteraction::connect_to_peers(
            peer_addrs,
            info_hash_raw,
            num_pieces,
            require_crypto,
            force_encrypt,
        )
        .await?;

        let mut active_connections = conn_result.connections;

        tracing::info!("[BT] Active connections: {}", active_connections.len());

        // Filter out self-connections and duplicate peer IDs.
        // Mirrors C++ DefaultBtInteractive::receiveHandshake() checks.
        let removed = filter_duplicate_peer_connections(&mut active_connections, &local_peer_id);
        if removed > 0 {
            tracing::info!(
                "[BT] Filtered {} invalid connections (self/duplicate), {} remaining",
                removed,
                active_connections.len()
            );
        }

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed or were filtered".into(),
                },
            ));
        }

        {
            let options = self.group.recover().options_arc();
            let config = ChokingConfig {
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10,
            };

            let mut algo = ChokingAlgorithm::new(config);

            for addr in peer_addrs {
                let socket_addr = std::net::SocketAddr::new(
                    addr.ip.parse().unwrap_or_else(|_| {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                    }),
                    addr.port,
                );
                let peer_stats = PeerStats::new([0u8; 20], socket_addr);
                algo.add_peer(peer_stats);
            }

            self.choking_algo = Some(algo);
            tracing::info!(
                "[BT] Choking algorithm initialized with {} peers",
                self.choking_algo.as_ref().unwrap().len()
            );
        }

        Ok(active_connections)
    }

    /// Check all tracked peers for snubbing (no data received within timeout).
    /// Called periodically from the download loop.
    pub(super) fn check_and_mark_snubbed_peers(
        &mut self,
        last_snub_check: &mut Instant,
        peer_last_data_time: &HashMap<usize, Instant>,
    ) {
        const SNUB_CHECK_INTERVAL_SECS: u64 = 10;
        const SNUB_TIMEOUT_SECS: u64 = 30;

        if last_snub_check.elapsed().as_secs() < SNUB_CHECK_INTERVAL_SECS {
            return;
        }
        *last_snub_check = Instant::now();

        let mut newly_snubbed = Vec::new();
        for (&peer_id, &last_time) in peer_last_data_time {
            if last_time.elapsed().as_secs() > SNUB_TIMEOUT_SECS {
                self.mark_peer_snubbed(peer_id);
                newly_snubbed.push(peer_id);
                debug!(
                    "[BT] Peer {} marked as snubbed (no data for {}s)",
                    peer_id,
                    last_time.elapsed().as_secs()
                );
            }
        }
        if !newly_snubbed.is_empty() {
            debug!(
                "[BT] Snub check: {} peers newly snubbed",
                newly_snubbed.len()
            );
        }

        // Also run the PeerStats-level snub check (timeout-based)
        let stats_snubbed = self.check_snubbed_peers();
        if !stats_snubbed.is_empty() {
            debug!(
                "[BT] PeerStats snub check: {} peers timed out",
                stats_snubbed.len()
            );
        }
    }

    /// Periodic tracker re-announce for peer discovery during download.
    ///
    /// C++ aria2 uses `TrackerWatcherCommand` which checks `BtAnnounce::isAnnounceReady()`
    /// on each iteration and dispatches a new announce when the interval has elapsed.
    /// This method replicates that behavior by checking the `TrackerAnnouncer` state
    /// machine and dispatching an announce if ready.
    ///
    /// Returns any newly discovered peers (may be empty if announce not ready yet).
    pub(super) async fn periodic_tracker_announce(
        &mut self,
        info_hash: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
    ) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
        let Some(ref mut announcer) = self.tracker_announcer else {
            return Vec::new();
        };

        if !announcer.is_announce_ready() {
            return Vec::new();
        }

        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();

        match announcer.announce(info_hash, &my_peer_id, downloaded, left, uploaded).await {
            Some(result) => {
                if result.peers.is_empty() {
                    debug!(
                        "[BT] Periodic tracker announce to {} returned no new peers",
                        result.tracker_url
                    );
                    Vec::new()
                } else {
                    info!(
                        "[BT] Periodic tracker announce discovered {} peers from {} (event={:?})",
                        result.peers.len(),
                        result.tracker_url,
                        result.event
                    );
                    result
                        .peers
                        .into_iter()
                        .map(|(ip, port)| {
                            aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port)
                        })
                        .collect()
                }
            }
            None => Vec::new(),
        }
    }
}
