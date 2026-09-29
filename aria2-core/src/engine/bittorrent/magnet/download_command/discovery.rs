//! Tracker and DHT peer discovery used while resolving magnet metadata.

use super::{MAX_MAGNET_TRACKER_PEERS, MAX_MAGNET_TRACKERS_TO_TRY, MagnetDownloadCommand};
use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
use crate::error::Result;
use crate::http::client_identity::ClientTlsConfig;
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
pub(super) fn metadata_tracker_urls(
    magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    options: &DownloadOptions,
) -> Vec<String> {
    let excluded = options.bt_exclude_tracker.as_deref().unwrap_or_default();
    if excluded.iter().any(|url| url == "*") {
        return Vec::new();
    }

    let mut urls = magnet.trackers.clone();
    if let Some(overrides) = options.bt_tracker.as_ref() {
        urls.extend(overrides.iter().cloned());
    }

    urls.into_iter()
        .map(|url| url.trim().to_owned())
        .filter(|url| {
            !url.is_empty()
                && matches!(
                    reqwest::Url::parse(url)
                        .ok()
                        .as_ref()
                        .map(reqwest::Url::scheme),
                    Some("http" | "https" | "udp" | "ws" | "wss")
                )
                && !excluded.iter().any(|excluded| excluded == url)
        })
        .fold(Vec::new(), |mut unique, url| {
            if !unique.iter().any(|known| known == &url) {
                unique.push(url);
            }
            unique
        })
}

pub(super) fn tracker_peer_socket_addr(ip: &str, port: u16) -> Option<SocketAddr> {
    ip.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

pub(super) fn append_unique_tracker_peers(
    discovered: &mut Vec<SocketAddr>,
    peers: Vec<SocketAddr>,
) {
    for peer in peers {
        if !discovered.contains(&peer) {
            discovered.push(peer);
            if discovered.len() >= MAX_MAGNET_TRACKER_PEERS {
                break;
            }
        }
    }
}

impl MagnetDownloadCommand {
    pub(super) fn clear_registered_dht_engines(&self) {
        if let Some(registry) = self.bt_registry.as_ref()
            && let Ok(mut registry) = registry.write()
        {
            let gid = self.group.recover().gid().value();
            for engine in self.dht_engines.iter() {
                registry.clear_dht_engine_for_gid_if(gid, engine);
            }
        }
    }

    pub(super) async fn ensure_dht_engines(&mut self, options: &DownloadOptions) -> Result<()> {
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

            if self
                .dht_engines
                .attach_global_if_present(self.bt_registry.as_ref(), gid, use_ipv6)
            {
                continue;
            }

            let config =
                crate::engine::bittorrent::dht::config::build_dht_engine_config_for_family(
                    options,
                    &self.outbound_network_policy,
                    use_ipv6,
                )
                .await?;
            match self
                .dht_engines
                .start_or_join(self.bt_registry.as_ref(), gid, config)
                .await
            {
                Ok(()) => info!(ipv6 = use_ipv6, "Magnet: DHT family engine ready"),
                Err(error) => {
                    warn!(ipv6 = use_ipv6, %error, "Magnet: DHT family engine start failed")
                }
            }
        }

        Ok(())
    }

    pub(super) async fn handoff_dht_engine_to_bt(
        &mut self,
        bt_command: &mut crate::engine::bittorrent::download::command::BtDownloadCommand,
    ) {
        let options = bt_command.group.recover().options().clone();
        let dht_enabled = !bt_command.is_private && (options.enable_dht || options.enable_dht6);
        if !dht_enabled {
            return;
        }
        bt_command.dht_engines = std::mem::take(&mut self.dht_engines);
    }

    pub(super) async fn discover_magnet_peers(
        &self,
        engines: &DhtEngineSet,
        info_hash: &[u8; 20],
    ) -> Vec<std::net::SocketAddr> {
        const MAX_ATTEMPTS: usize = 4;
        const RETRY_DELAYS: [Duration; 3] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ];

        if engines.is_empty() {
            return Vec::new();
        }
        let readiness = futures::future::join_all(engines.iter().map(|engine| async move {
            (
                engine.local_addr(),
                engine.wait_until_ready(Duration::from_secs(5)).await,
            )
        }))
        .await;
        for (address, result) in readiness {
            if let Err(error) = result {
                warn!(%address, %error, "Magnet: DHT family engine did not become ready before lookup");
            }
        }

        for attempt in 0..MAX_ATTEMPTS {
            match engines.find_peers(info_hash).await {
                Ok(result) if !result.peers.is_empty() => {
                    info!(
                        "Magnet: DHT discovered {} peers (contacted {} nodes, attempt {})",
                        result.peers.len(),
                        result.nodes_contacted,
                        attempt + 1
                    );
                    return result.peers;
                }
                Ok(result) => {
                    warn!(
                        "Magnet: DHT lookup found no peers (contacted {} nodes, attempt {}/{})",
                        result.nodes_contacted,
                        attempt + 1,
                        MAX_ATTEMPTS
                    );
                }
                Err(error) => {
                    warn!(
                        "Magnet: DHT find_peers failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_ATTEMPTS,
                        error
                    );
                }
            }

            if let Some(delay) = RETRY_DELAYS.get(attempt) {
                tokio::time::sleep(*delay).await;
            }
        }

        Vec::new()
    }

    pub(super) async fn discover_magnet_tracker_peers(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        options: &DownloadOptions,
    ) -> Vec<SocketAddr> {
        let tracker_urls = metadata_tracker_urls(magnet, options);
        if tracker_urls.is_empty() {
            return Vec::new();
        }

        let peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id_with_prefix(
            &options.peer_id_prefix,
        );
        let tracker_tls = ClientTlsConfig::from_download_options(options);
        let max_tracker_timeout = Duration::from_secs(options.bt_tracker_timeout.max(1));
        let connect_timeout = Duration::from_secs(options.bt_tracker_connect_timeout.max(1));
        let announce_port = options
            .listen_port
            .as_deref()
            .and_then(|value| value.split(['-', ',']).next())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        let mut discovered_peers = Vec::new();

        for tracker_url in tracker_urls.into_iter().take(MAX_MAGNET_TRACKERS_TO_TRY) {
            let tracker_scheme = reqwest::Url::parse(&tracker_url)
                .ok()
                .map(|url| url.scheme().to_owned());
            if matches!(tracker_scheme.as_deref(), Some("ws" | "wss")) {
                match crate::engine::bittorrent::tracker::websocket::announce_with_policy(
                    &tracker_url,
                    crate::engine::bittorrent::tracker::websocket::AnnounceRequest {
                        info_hash: &magnet.info_hash,
                        peer_id: &peer_id,
                        downloaded: 0,
                        left: magnet.exact_length.unwrap_or(0),
                        uploaded: 0,
                        numwant: 50,
                        port: announce_port,
                        event: crate::engine::bittorrent::tracker::communication::AnnounceEvent::Started,
                        options,
                    },
                    &self.outbound_network_policy,
                )
                .await
                {
                    Ok(response) if !response.peers.is_empty() => {
                        info!(
                            tracker = %tracker_url,
                            peers = response.peers.len(),
                            "WebSocket tracker announce returned peers"
                        );
                        append_unique_tracker_peers(&mut discovered_peers, response.peers);
                    }
                    Ok(_) => {
                        warn!(tracker = %tracker_url, "WebSocket tracker returned no TCP peers");
                    }
                    Err(error) => {
                        warn!(tracker = %tracker_url, %error, "WebSocket tracker announce failed");
                    }
                }
                if discovered_peers.len() >= MAX_MAGNET_TRACKER_PEERS {
                    break;
                }
                continue;
            }

            let tracker_tiers = vec![vec![tracker_url.clone()]];
            let mut announcer = TrackerAnnouncer::new(&tracker_tiers, &None);
            announcer.set_outbound_network_policy(Arc::clone(&self.outbound_network_policy));
            announcer.set_http_tls_config(tracker_tls.clone());
            announcer.set_timeouts(max_tracker_timeout, connect_timeout);
            announcer.set_tcp_port(announce_port);

            let result = announcer
                .announce(
                    &magnet.info_hash,
                    &peer_id,
                    0,
                    magnet.exact_length.unwrap_or(0),
                    0,
                )
                .await;

            let Some(result) = result else {
                warn!(tracker = %tracker_url, "Magnet tracker announce failed");
                continue;
            };

            let peers: Vec<SocketAddr> = result
                .peers
                .iter()
                .filter_map(|(ip, port)| tracker_peer_socket_addr(ip, *port))
                .collect();
            info!(
                tracker = %tracker_url,
                peers = peers.len(),
                "Magnet tracker announce returned peers"
            );
            append_unique_tracker_peers(&mut discovered_peers, peers);

            if discovered_peers.len() >= MAX_MAGNET_TRACKER_PEERS {
                break;
            }
        }

        if !discovered_peers.is_empty() {
            info!(
                peers = discovered_peers.len(),
                "Magnet tracker peer discovery completed"
            );
        }
        discovered_peers
    }

    pub(super) async fn shutdown_dht_engine(&mut self) {
        self.clear_registered_dht_engines();
        for engine in std::mem::take(&mut self.dht_engines).into_vec() {
            let is_shared = self
                .bt_registry
                .as_ref()
                .and_then(|registry| registry.read().ok())
                .is_some_and(|registry| registry.is_global_dht_engine(&engine));
            if !is_shared {
                engine.shutdown_async().await;
            }
        }
    }
}
