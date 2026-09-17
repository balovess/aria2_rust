use std::collections::HashSet;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::error::{Aria2Error, FatalError, Result};
use crate::http::client_identity::ClientTlsConfig;
use crate::util::rwlock_ext::RwLockRecover;

pub(super) fn filter_tracker_tiers(
    tiers: Vec<Vec<String>>,
    excluded: &[String],
) -> Vec<Vec<String>> {
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

pub(super) fn prepare_tracker_tiers(
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

impl BtDownloadCommand {
    /// Discover peers via tracker announce (HTTP/UDP), DHT, public trackers, and LPD.
    ///
    /// Uses the `TrackerAnnouncer` state machine for proper HTTP/UDP dispatch,
    /// tier rotation, and event management (Started → Downloading → Completed/Stopped).
    pub(in crate::engine::bt_download_execute::execute) async fn discover_peers(
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
        let mut public_tracker_urls = HashSet::new();
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
                public_tracker_urls.insert(url.clone());
                tracker_tiers.push(vec![url]);
            }
        }

        tracker_tiers = super::super::deduplicate_tracker_tiers(tracker_tiers);
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
            announcer.set_public_tracker_catalog(catalog, public_tracker_urls);
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
            if self.lpd_registered_info_hash.as_ref() != Some(info_hash_raw) {
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
                self.lpd_registered_info_hash = Some(*info_hash_raw);
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

    /// Periodic tracker re-announce for peer discovery during download.
    ///
    /// C++ aria2 uses `TrackerWatcherCommand` which checks `BtAnnounce::isAnnounceReady()`
    /// on each iteration and dispatches a new announce when the interval has elapsed.
    /// This method replicates that behavior by checking the `TrackerAnnouncer` state
    /// machine and dispatching an announce if ready.
    ///
    /// Returns any newly discovered peers (may be empty if announce not ready yet).
    pub(in crate::engine::bt_download_execute::execute) async fn periodic_tracker_announce(
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
