use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::config::parse_integer_segments;
use crate::engine::bt_download_command::{BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bt_download_execute::types::PeerKey;
use crate::engine::bt_handshake_validation::filter_duplicate_peer_connections;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerCryptoPolicy;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::peer_stats::PeerStats;
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::client_identity::ClientTlsConfig;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    fn return_checked_out_peers(
        &self,
        checked_out: &[(
            aria2_protocol::bittorrent::peer::connection::PeerAddr,
            crate::engine::bt_peer_storage::PeerEntry,
        )],
    ) {
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, peer) in checked_out {
            storage.return_peer(peer);
        }
    }

    fn reconcile_checked_out_peers(
        &self,
        checked_out: &[(
            aria2_protocol::bittorrent::peer::connection::PeerAddr,
            crate::engine::bt_peer_storage::PeerEntry,
        )],
        active_connections: &[BtPeerConn],
    ) {
        let active: HashSet<_> = active_connections
            .iter()
            .map(|peer| (peer.ip_addr.clone(), peer.port))
            .collect();
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (address, peer) in checked_out {
            if active.contains(&(address.ip.clone(), address.port)) {
                storage.set_peer_active(&peer.ip, peer.port, true);
            } else {
                storage.return_peer(peer);
            }
        }
    }

    pub(super) fn return_all_checked_out_peers(&self) {
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let peers: Vec<_> = storage.used_peers().iter().cloned().collect();
        for peer in peers {
            storage.return_peer(&peer);
        }
    }

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
        let my_peer_id = self.local_peer_id;

        // Initialize the unified TrackerAnnouncer from the torrent's announce list.
        // This replaces the separate HTTP-only + ad-hoc UDP approach with a single
        // state machine that properly routes HTTP vs UDP based on URL scheme.
        // `--bt-tracker` overrides the torrent's own announce list
        // (C++: option value replaces announce URLs).
        let tracker_override: Option<Vec<String>> = {
            let g = self.group.recover();
            g.options().bt_tracker.clone()
        };
        let mut tracker_tiers = match tracker_override {
            Some(list) if !list.is_empty() => {
                info!(
                    count = list.len(),
                    "Using user-specified trackers from --bt-tracker"
                );
                list.into_iter().map(|u| vec![u]).collect()
            }
            _ => {
                let mut tiers = meta.announce_list.clone();
                if tiers.is_empty() && !meta.announce.is_empty() {
                    tiers.push(vec![meta.announce.clone()]);
                }
                tiers
            }
        };

        let enable_public_trackers =
            { self.group.recover().options().enable_public_trackers } && !self.is_private;
        let tracker_tls = {
            let group = self.group.recover();
            ClientTlsConfig::from_download_options(group.options())
        };
        let public_tracker_catalog = self.public_trackers.clone();
        if enable_public_trackers && let Some(catalog) = public_tracker_catalog.as_ref() {
            let public_entries = catalog.available_snapshot().await;
            let existing_urls: HashSet<String> = tracker_tiers
                .iter()
                .flat_map(|tier| tier.iter().cloned())
                .collect();
            let public_urls: Vec<String> = public_entries
                .iter()
                .filter(|entry| {
                    entry.protocol
                        != aria2_protocol::bittorrent::tracker::public_list::TrackerProtocol::Wss
                })
                .map(|entry| entry.url.clone())
                .filter(|url| !existing_urls.contains(url))
                .take(MAX_PUBLIC_TRACKERS_TO_TRY)
                .collect();
            for url in public_urls {
                self.public_tracker_urls.insert(url.clone());
                tracker_tiers.push(vec![url]);
            }
        }

        tracker_tiers = super::deduplicate_tracker_tiers(tracker_tiers);
        let mut announcer = TrackerAnnouncer::new(&tracker_tiers, &Some(meta.announce.clone()));
        announcer.set_http_tls_config(tracker_tls);
        if let Some(catalog) = public_tracker_catalog {
            announcer.set_public_tracker_catalog(catalog, self.public_tracker_urls.clone());
        }

        // Set up UDP client for UDP tracker support
        if let Ok(udp) = UdpTrackerClient::new(0).await {
            let shared = std::sync::Arc::new(tokio::sync::Mutex::new(udp));
            self.udp_client = Some(std::sync::Arc::clone(&shared));
            announcer.set_udp_client(shared);
        }

        // The listener is created before discovery, matching BtSetup's order in
        // the original engine. Advertise its actual port in every announce.
        announcer.set_tcp_port(self.listen_port);

        let mut peer_addrs: Vec<(String, u16)> = Vec::new();

        // Try tracker announces through the state machine (handles both HTTP and UDP)
        let mut announce_attempts = 0;
        const MAX_ANNOUNCE_ATTEMPTS: usize = MAX_PUBLIC_TRACKERS_TO_TRY;

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

        // Store the announcer for periodic re-announce during download
        self.tracker_announcer = Some(announcer);

        let mut peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
            peer_addrs
                .into_iter()
                .filter(|(ip, _)| !self.is_peer_temporarily_rejected(ip))
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
            let dht_port = { self.group.recover().options().dht_listen_port.clone() };
            let dht_file_path = { self.group.recover().options().dht_file_path.clone() };
            let dht_entry_points = { self.group.recover().options().dht_entry_point.clone() };
            let dht_ports = dht_port
                .as_deref()
                .map(|value| {
                    parse_integer_segments(value, 1024, u16::MAX as i64).map(|ranges| {
                        ranges
                            .into_iter()
                            .flat_map(|range| range.map(|port| port as u16))
                            .collect::<Vec<_>>()
                    })
                })
                .transpose()
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "invalid dht-listen-port: {error}"
                    )))
                })?;

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
                port: dht_ports
                    .as_ref()
                    .and_then(|ports| ports.first().copied())
                    .unwrap_or(0),
                port_range: dht_ports,
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

        if let Some(ref engine) = self.dht_engine {
            match engine.find_peers(info_hash_raw).await {
                Ok(result) => {
                    if !result.peers.is_empty() {
                        let before = peer_addrs.len();
                        for addr in &result.peers {
                            let ip_str = addr.ip().to_string();
                            let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                                &ip_str,
                                addr.port(),
                            );
                            if !self.is_peer_temporarily_rejected(&paddr.ip)
                                && !peer_addrs
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
                Err(error) => {
                    debug!(error = %error, "[BT] Initial DHT peer lookup failed");
                }
            }
        }

        // BEP 0027 (Private Torrent): public tracker announcement is forbidden
        // for private torrents because it would leak the info_hash to trackers
        // not explicitly listed in the torrent's announce list.
        if self.is_private {
            info!("[BT] Private torrent: public trackers disabled (BEP 0027)");
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
                    if !self.is_peer_temporarily_rejected(&paddr.ip)
                        && !peer_addrs
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

    /// Return the number of peers currently owned by this torrent's storage.
    ///
    /// This is the Rust equivalent of C++ `PeerStorage::countAllPeer()` and
    /// intentionally includes both queued and connected peers.
    pub(super) fn tracked_peer_count(&self) -> usize {
        self.peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .count_all_peers()
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
        piece_length: u32,
        total_size: u64,
    ) -> Result<Vec<BtPeerConn>> {
        let crypto_policy = {
            let group = self.group.recover();
            BtPeerCryptoPolicy {
                require_mse: group.options().bt_require_crypto || group.options().bt_force_encrypt,
                force_encryption: group.options().bt_force_encrypt,
                prefer_encryption: group.effective_option_snapshot().is_some_and(|snapshot| {
                    snapshot
                        .get("bt-min-crypto-level")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|level| level.eq_ignore_ascii_case("arc4"))
                }) || group.options().bt_force_encrypt,
            }
        };

        // Generate our local peer ID for this session. This is used for
        // self-connection detection (C++ bittorrent::getStaticPeerId()).
        // Note: C++ generates the static peer ID once per session; here we
        // generate it at connection time. For future sessions, this should
        // be a per-session singleton.
        let local_peer_id = self.local_peer_id;

        let max_peers = self.group.recover().options().bt_max_peers;
        self.peer_coordinator.set_max_peers(max_peers);
        self.bt_runtime.set_max_peers(max_peers);
        self.bt_runtime.set_connections(0);
        let remaining_slots = if self.bt_runtime.less_than_max_peers() {
            max_peers.saturating_sub(self.bt_runtime.connections())
        } else {
            0
        };
        let peer_limit = if max_peers == 0 {
            peer_addrs.len()
        } else {
            remaining_slots.min(peer_addrs.len())
        };
        let mut eligible_peers = Vec::with_capacity(peer_limit);
        for peer in peer_addrs.iter().take(peer_limit) {
            if let Ok(ip) = peer.ip.parse::<std::net::IpAddr>()
                && self.is_peer_temporarily_rejected(&ip.to_string())
            {
                tracing::debug!(peer = %ip, port = peer.port, "Skipping temporarily rejected peer");
                continue;
            }
            eligible_peers.push(peer.clone());
        }

        let mut seen = HashSet::with_capacity(eligible_peers.len());
        eligible_peers.retain(|peer| seen.insert((peer.ip.clone(), peer.port)));
        let caretaker_id = self.group.recover().gid().value();
        let mut checked_out = Vec::with_capacity(eligible_peers.len());
        {
            let mut storage = self
                .peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for peer in eligible_peers {
                let entry =
                    crate::engine::bt_peer_storage::PeerEntry::new(peer.ip.clone(), peer.port);
                if let Some(checked_peer) = storage.add_and_checkout_peer(entry, caretaker_id) {
                    checked_out.push((peer, checked_peer));
                }
            }
        }
        let eligible_peers: Vec<_> = checked_out.iter().map(|(peer, _)| peer.clone()).collect();
        if eligible_peers.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No available peers after PeerStorage checkout".into(),
                },
            ));
        }

        let conn_result = match BtPeerInteraction::connect_to_peers(
            &eligible_peers,
            info_hash_raw,
            num_pieces,
            piece_length,
            total_size,
            crypto_policy,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                self.return_checked_out_peers(&checked_out);
                return Err(error);
            }
        };

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
        self.reconcile_checked_out_peers(&checked_out, &active_connections);
        self.bt_runtime.set_connections(active_connections.len());

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

            for conn in &active_connections {
                let Ok(ip) = conn.ip_addr.parse::<std::net::IpAddr>() else {
                    warn!(peer = %conn.ip_addr, port = conn.port, "Skipping active peer with invalid IP");
                    continue;
                };
                let socket_addr = std::net::SocketAddr::new(ip, conn.port);
                let peer_stats = PeerStats::new(conn.peer_id.unwrap_or([0u8; 20]), socket_addr);
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
        peer_last_data_time: &HashMap<PeerKey, Instant>,
        active_connections: &[BtPeerConn],
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
                if let Some(index) = active_connections
                    .iter()
                    .position(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port) == Some(peer_id))
                {
                    self.mark_peer_snubbed(index);
                }
                newly_snubbed.push(peer_id);
                debug!(
                    "[BT] Peer {} marked as snubbed (no data for {}s)",
                    peer_id.address(),
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

    /// Update tracker demand from the live connection count.
    ///
    /// The C++ `BtRuntime::lessThanMinPeers()` is derived from active peer
    /// commands and its configured max-peer limit, not from the last tracker
    /// response. Keep the Rust announce state synchronized at the same boundary.
    pub(super) fn update_tracker_peer_state(&mut self, active_connections: usize) {
        let max_peers = self.group.recover().options().bt_max_peers;
        self.bt_runtime.set_max_peers(max_peers);
        self.bt_runtime.set_connections(active_connections);
        if let Some(announcer) = self.tracker_announcer.as_mut() {
            announcer.set_less_than_min_peers(self.bt_runtime.less_than_min_peers());
        }
    }

    pub(super) fn should_discover_more_peers(&self, active_connections: usize) -> bool {
        self.peer_coordinator.should_replenish(active_connections)
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

        let my_peer_id = self.local_peer_id;

        match announcer
            .announce(info_hash, &my_peer_id, downloaded, left, uploaded)
            .await
        {
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
