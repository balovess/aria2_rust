use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bt_download_execute::types::PeerKey;
use crate::engine::bt_handshake_validation::filter_duplicate_peer_connections;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::{BtPeerConnectionOptions, BtPeerInteraction};
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::peer_stats::PeerStats;
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::http::client_identity::ClientTlsConfig;
use crate::util::rwlock_ext::RwLockRecover;

fn filter_tracker_tiers(tiers: Vec<Vec<String>>, excluded: &[String]) -> Vec<Vec<String>> {
    if excluded.iter().any(|url| url == "*") {
        return Vec::new();
    }
    if excluded.is_empty() {
        return tiers;
    }

    tiers
        .into_iter()
        .filter_map(|tier| {
            let remaining = tier
                .into_iter()
                .filter(|url| !excluded.iter().any(|excluded| excluded == url))
                .collect::<Vec<_>>();
            (!remaining.is_empty()).then_some(remaining)
        })
        .collect()
}

fn prepare_tracker_tiers(
    mut tiers: Vec<Vec<String>>,
    announce: &str,
    tracker_override: Option<Vec<String>>,
    excluded: &[String],
) -> Vec<Vec<String>> {
    if tiers.is_empty() && !announce.is_empty() {
        tiers.push(vec![announce.to_string()]);
    }
    let mut tiers = filter_tracker_tiers(tiers, excluded);
    if let Some(list) = tracker_override.filter(|list| !list.is_empty()) {
        info!(
            count = list.len(),
            "Appending user-specified trackers from --bt-tracker"
        );
        tiers.extend(list.into_iter().map(|url| vec![url]));
    }
    tiers
}

fn effective_peer_speed_threshold(configured: u64, max_download_limit: Option<u64>) -> u64 {
    match max_download_limit.filter(|limit| *limit > 0) {
        Some(limit) => configured.min(limit),
        None => configured,
    }
}

fn download_speed_is_below_peer_request_limit(
    current_speed: u64,
    configured: u64,
    max_download_limit: Option<u64>,
) -> bool {
    let threshold = effective_peer_speed_threshold(configured, max_download_limit);
    threshold > 0 && current_speed < threshold
}

impl BtDownloadCommand {
    pub(super) fn peer_exchange_enabled(&self) -> bool {
        !self.is_private && self.group.recover().options().enable_peer_exchange
    }

    pub(super) fn apply_peer_exchange_policy(&self, conn: &mut BtPeerConn) {
        conn.set_pex_enabled(self.peer_exchange_enabled());
    }

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

    async fn persist_checkpoint_for_halt(&mut self) -> Result<()> {
        let bitfield = self.group.recover().get_bt_bitfield();
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            return Ok(());
        };
        let bitfield = bitfield
            .or_else(|| checkpoint.bitfield().map(ToOwned::to_owned))
            .unwrap_or_default();
        checkpoint.save(&bitfield, self.completed_bytes).await
    }

    async fn announce_stopped_for_halt(&mut self, info_hash: &[u8; 20], total_size: u64) {
        if let Some(announcer) = self.tracker_announcer.as_mut() {
            announcer
                .announce_stopped(
                    info_hash,
                    &self.local_peer_id,
                    self.completed_bytes,
                    total_size.saturating_sub(self.completed_bytes),
                    self.total_uploaded,
                )
                .await;
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
        // C++ first removes excluded torrent trackers and then appends each
        // `--bt-tracker` URL as its own tier.
        let (
            tracker_override,
            excluded_trackers,
            tracker_timeout,
            tracker_connect_timeout,
            tracker_stopped_timeout,
            tracker_interval,
            external_ip,
            force_encryption,
        ) = {
            let g = self.group.recover();
            (
                g.options().bt_tracker.clone(),
                g.options().bt_exclude_tracker.clone().unwrap_or_default(),
                g.options().bt_tracker_timeout,
                g.options().bt_tracker_connect_timeout,
                g.options().bt_tracker_stopped_timeout,
                g.options().bt_tracker_interval,
                g.options().bt_external_ip.clone(),
                g.options().bt_force_encrypt || g.options().bt_require_crypto,
            )
        };
        let mut tracker_tiers = prepare_tracker_tiers(
            meta.announce_list.clone(),
            &meta.announce,
            tracker_override,
            &excluded_trackers,
        );

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
        let mut announcer = TrackerAnnouncer::new(&tracker_tiers, &None);
        announcer.set_http_tls_config(tracker_tls);
        announcer.set_timeouts(
            Duration::from_secs(tracker_timeout),
            Duration::from_secs(tracker_connect_timeout),
        );
        announcer.set_stopped_timeout(Duration::from_secs(tracker_stopped_timeout));
        announcer.set_user_defined_interval(Duration::from_secs(tracker_interval));
        announcer.set_announce_options(force_encryption, external_ip);
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

        // Register before reading the peer registry and send one announce
        // immediately. The registration must not depend on another peer
        // already being present: the first announce is how this torrent
        // becomes discoverable by another client on the LAN.
        if !self.is_private
            && self.group.recover().options().bt_enable_lpd
            && let Some(lpd) = self.lpd_manager.as_ref().cloned()
        {
            let info_hash_hex = hex::encode(*info_hash_raw);
            if self.lpd_registered_info_hash.as_deref() != Some(info_hash_hex.as_str()) {
                let registration = if self.listen_port > 0 {
                    lpd.register_torrent_with_port(&info_hash_hex, false, self.listen_port)
                        .await
                } else {
                    // Tests and callers that do not own a real BT listener can
                    // still receive LPD discoveries, but must not advertise an
                    // unusable TCP port.
                    lpd.register_torrent(&info_hash_hex, false).await
                };
                registration.map_err(|error| {
                    Aria2Error::Fatal(FatalError::Config(format!(
                        "LPD torrent registration failed: {error}"
                    )))
                })?;
                self.lpd_registered_info_hash = Some(info_hash_hex.clone());
            }
            lpd.ensure_runtime_started().await;
            if self.listen_port > 0
                && let Err(error) = lpd.announce_torrent(&info_hash_hex, self.listen_port).await
            {
                warn!(%error, "Initial LPD announce failed");
            }

            let lpd_peers = lpd.get_peers_for(&info_hash_hex).await;
            if !lpd_peers.is_empty() {
                let before = peer_addrs.len();
                for lpd_peer in &lpd_peers {
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                        &lpd_peer.addr.to_string(),
                        lpd_peer.port,
                    );
                    if !self.is_peer_temporarily_rejected(&paddr.ip)
                        && !peer_addrs
                            .iter()
                            .any(|peer| peer.ip == paddr.ip && peer.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }
                info!(
                    lpd_count = lpd_peers.len(),
                    total_added = peer_addrs.len() - before,
                    "LPD discovered local peers"
                );
            } else {
                debug!("LPD no local peers found for this torrent");
            }
        }

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
            let options = { self.group.recover().options().clone() };
            let dht_config = crate::engine::dht_config::build_dht_engine_config(&options).await?;

            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
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
        // BEP 0027: private torrents never enter the LPD branch above.
        if self.is_private && self.lpd_manager.is_some() {
            info!("[BT] Private torrent: LPD disabled (BEP 0027)");
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
        let connection_options = {
            let group = self.group.recover();
            BtPeerConnectionOptions::from_download_options(group.options(), self.local_peer_id)
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

        if self.group.recover().is_halt_requested() {
            self.return_checked_out_peers(&checked_out);
            self.persist_checkpoint_for_halt().await?;
            self.announce_stopped_for_halt(info_hash_raw, total_size)
                .await;
            return Err(Aria2Error::DownloadFailed(
                "BitTorrent download halted".into(),
            ));
        }

        // Keep the initial connection batch cancellable. A tracker can return
        // many slow peers, and waiting for every handshake would otherwise
        // delay graceful shutdown while new sockets continue to open.
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        let mut connect_future = Box::pin(BtPeerInteraction::connect_to_peers(
            &eligible_peers,
            info_hash_raw,
            num_pieces,
            piece_length,
            total_size,
            &connection_options,
            self.utp_socket.clone(),
        ));
        let conn_result = loop {
            tokio::select! {
                result = &mut connect_future => break result,
                _ = &mut lifecycle_wait => {
                    if self.group.recover().is_halt_requested() {
                        self.return_checked_out_peers(&checked_out);
                        self.persist_checkpoint_for_halt().await?;
                        self.announce_stopped_for_halt(info_hash_raw, total_size)
                            .await;
                        return Err(Aria2Error::DownloadFailed(
                            "BitTorrent download halted".into(),
                        ));
                    }
                    lifecycle_wait.set(lifecycle_notify.notified());
                }
            }
        };
        let conn_result = match conn_result {
            Ok(result) => result,
            Err(error) => {
                self.return_checked_out_peers(&checked_out);
                return Err(error);
            }
        };

        let mut active_connections = conn_result.connections;
        for conn in &mut active_connections {
            self.apply_peer_exchange_policy(conn);
        }

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
        self.group
            .recover()
            .set_bt_connection_count(active_connections.len());

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
        if self.peer_coordinator.should_replenish(active_connections) {
            return true;
        }

        let group = self.group.recover();
        download_speed_is_below_peer_request_limit(
            group.download_speed(),
            group.options().bt_request_peer_speed_limit,
            group.options().max_download_limit,
        )
    }

    pub(super) fn should_admit_incoming_peer(&self, active_connections: usize) -> bool {
        let group = self.group.recover();
        if group.options().bt_max_peers == 0 || active_connections < group.options().bt_max_peers {
            return true;
        }

        download_speed_is_below_peer_request_limit(
            group.download_speed(),
            group.options().bt_request_peer_speed_limit,
            group.options().max_download_limit,
        )
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

#[cfg(test)]
mod tests {
    use super::{
        download_speed_is_below_peer_request_limit, effective_peer_speed_threshold,
        prepare_tracker_tiers,
    };
    use crate::engine::bt_download_command::BtDownloadCommand;
    use crate::engine::bt_download_command_tests::build_test_torrent;
    use crate::engine::lpd_manager::LpdManager;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use std::sync::Arc;

    #[tokio::test]
    async fn lpd_registers_public_torrent_before_empty_peer_results() {
        let torrent = build_test_torrent();
        let meta =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent).unwrap();
        let options = DownloadOptions {
            bt_enable_lpd: true,
            enable_dht: false,
            enable_public_trackers: false,
            bt_exclude_tracker: Some(vec!["*".to_string()]),
            ..DownloadOptions::default()
        };
        let mut command = BtDownloadCommand::new(GroupId::new(7001), &torrent, &options, None)
            .expect("test torrent should construct");
        let manager = Arc::new(LpdManager::new());
        command.set_lpd_manager(Arc::clone(&manager));

        let peers = command
            .discover_peers(&meta, meta.total_size(), &meta.info_hash.bytes)
            .await
            .expect("discovery without network trackers should succeed");

        assert!(peers.is_empty());
        assert!(
            manager
                .active_hashes
                .read()
                .await
                .contains(&meta.info_hash.as_hex()),
            "LPD must register a public torrent even when no peer is discovered"
        );
    }

    #[test]
    fn tracker_exclusions_and_user_trackers_follow_announce_policy() {
        let tiers = prepare_tracker_tiers(
            vec![vec![
                "http://torrent-one.test/announce".to_string(),
                "http://torrent-two.test/announce".to_string(),
            ]],
            "",
            Some(vec!["http://custom.test/announce".to_string()]),
            &["http://torrent-one.test/announce".to_string()],
        );

        assert_eq!(
            tiers,
            vec![
                vec!["http://torrent-two.test/announce".to_string()],
                vec!["http://custom.test/announce".to_string()],
            ]
        );
    }

    #[test]
    fn wildcard_tracker_exclusion_removes_torrent_trackers_but_keeps_override() {
        let tiers = prepare_tracker_tiers(
            vec![vec!["http://torrent.test/announce".to_string()]],
            "",
            Some(vec!["http://custom.test/announce".to_string()]),
            &["*".to_string()],
        );

        assert_eq!(tiers, vec![vec!["http://custom.test/announce".to_string()]]);
    }

    #[test]
    fn peer_speed_threshold_is_clamped_by_download_limit() {
        assert_eq!(
            effective_peer_speed_threshold(50 * 1024, Some(20 * 1024)),
            20 * 1024
        );
        assert_eq!(
            effective_peer_speed_threshold(50 * 1024, Some(0)),
            50 * 1024
        );
        assert_eq!(effective_peer_speed_threshold(50 * 1024, None), 50 * 1024);
    }

    #[test]
    fn low_peer_speed_requests_more_peers_but_zero_disables_policy() {
        assert!(download_speed_is_below_peer_request_limit(
            10 * 1024,
            50 * 1024,
            None,
        ));
        assert!(!download_speed_is_below_peer_request_limit(
            50 * 1024,
            50 * 1024,
            None,
        ));
        assert!(!download_speed_is_below_peer_request_limit(0, 0, None));
    }
}
