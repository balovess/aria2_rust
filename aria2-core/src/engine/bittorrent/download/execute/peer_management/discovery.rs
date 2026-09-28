use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::engine::bittorrent::download::command::{BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY};
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
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
    /// Discover peers via tracker announce (HTTP/WebSocket/UDP), DHT, public trackers, and LPD.
    ///
    /// Uses the `TrackerAnnouncer` state machine for proper HTTP/WebSocket/UDP dispatch,
    /// tier rotation, and event management (Started → Downloading → Completed/Stopped).
    #[cfg(test)]
    pub(in crate::engine::bittorrent::download::execute) async fn discover_peers(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        total_size: u64,
        info_hash_raw: &[u8; 20],
    ) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
        self.discover_peers_with_events(meta, total_size, info_hash_raw, None)
            .await
    }

    pub(in crate::engine::bittorrent::download::execute) async fn discover_peers_with_events(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        total_size: u64,
        info_hash_raw: &[u8; 20],
        peer_event_tx: Option<
            tokio::sync::mpsc::Sender<crate::engine::bittorrent::peer::message_handler::PeerEvent>,
        >,
    ) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
        // Initialize the unified TrackerAnnouncer from the torrent's announce list.
        // This replaces the separate HTTP-only + ad-hoc UDP approach with a single
        // state machine that properly routes HTTP, WebSocket, and UDP based on
        // URL scheme.
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
            websocket_options,
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
                g.options().clone(),
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
                .map(|entry| entry.url.clone())
                .filter(|url| {
                    !existing_urls.contains(url)
                        && !excluded_trackers
                            .iter()
                            .any(|excluded| excluded == "*" || excluded == url)
                })
                .take(MAX_PUBLIC_TRACKERS_TO_TRY)
                .collect();
            for url in public_urls {
                public_tracker_urls.insert(url.clone());
                tracker_tiers.push(vec![url]);
            }
        }

        tracker_tiers = super::super::deduplicate_tracker_tiers(tracker_tiers);
        let mut announcer = TrackerAnnouncer::new(&tracker_tiers, &None);
        announcer.set_outbound_network_policy(Arc::clone(&self.outbound_network_policy));
        announcer.set_excluded_tracker_urls(excluded_trackers.clone());
        announcer.set_http_tls_config(tracker_tls);
        announcer.set_websocket_options(&websocket_options);
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

        // The listener is created before discovery so incoming peers can join
        // as soon as discovery starts. Advertise its actual port in every announce.
        announcer.set_tcp_port(self.listen_port);
        if let Some(runtime) = self.tracker_runtime.clone() {
            announcer.set_runtime_snapshot(runtime);
        }

        let (tracker_actor, peer_addrs) =
            crate::engine::bittorrent::download::execute::BtTrackerAnnouncerActor::start(
                announcer,
                *info_hash_raw,
                self.local_peer_id,
                total_size,
                Arc::clone(&self.progress),
                Arc::clone(&self.bt_runtime),
                enable_public_trackers,
                peer_event_tx,
            )
            .await;
        self.tracker_actor = Some(tracker_actor);

        let mut peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
            peer_addrs
                .into_iter()
                .filter(|peer| !self.is_peer_temporarily_rejected(&peer.ip))
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
        let options = { self.group.recover().options().clone() };
        if self.is_private {
            info!("[BT] Private torrent: DHT disabled (BEP 0027)");
        }
        if !self.is_private {
            let gid = self.group.recover().gid().value();
            for (use_ipv6, enabled) in [(false, options.enable_dht), (true, options.enable_dht6)] {
                let family_present = if use_ipv6 {
                    self.dht_engines.ipv6().is_some()
                } else {
                    self.dht_engines.ipv4().is_some()
                };
                if !enabled || family_present {
                    continue;
                }

                if self.dht_engines.attach_global_if_present(
                    self.bt_registry.as_ref(),
                    gid,
                    use_ipv6,
                ) {
                    continue;
                }

                let dht_config =
                    crate::engine::bittorrent::dht::config::build_dht_engine_config_for_family(
                        &options,
                        &self.outbound_network_policy,
                        use_ipv6,
                    )
                    .await?;
                match self
                    .dht_engines
                    .start_or_join(self.bt_registry.as_ref(), gid, dht_config)
                    .await
                {
                    Ok(()) => tracing::info!(ipv6 = use_ipv6, "[BT] DHT family engine ready"),
                    Err(error) => {
                        warn!(ipv6 = use_ipv6, %error, "[BT] DHT family engine start failed")
                    }
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
}
