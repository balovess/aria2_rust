use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::error::{Aria2Error, FatalError};
use crate::http::client_identity::ClientTlsConfig;
use crate::util::rwlock_ext::RwLockRecover;

use super::environment::parse_listen_ports;
use super::types::DiscoveredPeer;

/// Torrent-scoped swarm state retained from peer discovery through seeding.
pub(super) struct TorrentSession {
    /// Discovered endpoints, not live connections. Once dialed, a connection
    /// moves into this torrent's `PeerSwarm` when its actor context is ready.
    pub(super) initial_peers: Vec<DiscoveredPeer>,
    pub(super) network_info_hash: [u8; 20],
    /// Registry handed directly from download-session lifetime into seeding.
    pub(super) swarm: PeerSwarm,
    /// One task-lifetime upload counter shared by download and seed actors.
    pub(super) upload_counter: Arc<std::sync::atomic::AtomicU64>,
    pub(super) web_seed_manager:
        Option<Arc<crate::engine::bittorrent::download::web_seed::WebSeedManager>>,
    pub(super) last_pex_send: Instant,
}

impl BtDownloadCommand {
    pub(super) async fn prepare_torrent_session(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        network_info_hash: [u8; 20],
    ) -> crate::error::Result<TorrentSession> {
        // Register this torrent on the engine-owned listener before discovery
        // so one socket can serve all active torrents and route by info-hash.
        if self.incoming_peers.is_none() {
            let listener_manager = self.bt_listener.clone().ok_or_else(|| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: "BitTorrent listener manager is not configured".to_string(),
                })
            })?;
            let (listen_ports, max_peers, caretaker_id, disable_ipv6, crypto_policy) = {
                let group = self.group.recover();
                let ports = group
                    .options()
                    .listen_port
                    .as_deref()
                    .map(parse_listen_ports)
                    .transpose()
                    .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
                    .unwrap_or_else(|| vec![0]);
                (
                    ports,
                    group.options().bt_max_peers,
                    group.gid().value(),
                    group.options().disable_ipv6,
                    aria2_protocol::bittorrent::peer::incoming::IncomingCryptoPolicy {
                        reject_plain: group.options().bt_force_encrypt
                            || group.options().bt_require_crypto,
                        force_encryption: group.options().bt_force_encrypt,
                        prefer_encryption: group
                            .options()
                            .bt_min_crypto_level
                            .eq_ignore_ascii_case("arc4")
                            || group.options().bt_force_encrypt,
                    },
                )
            };
            let dht_enabled_ipv4 = !self.is_private && self.dht_engines.ipv4().is_some();
            let dht_enabled_ipv6 = !self.is_private && self.dht_engines.ipv6().is_some();
            let max_peers_state = self.bt_runtime.max_peers_state();
            let register = |bind_ip: std::net::IpAddr| {
                listener_manager.register_with_max_peers(
                    crate::engine::bittorrent::peer::listener::BtPeerRouteConfig {
                        bind_ip,
                        ports: listen_ports.clone(),
                        info_hash: network_info_hash,
                        info_hash_v2: meta.info_hash_v2,
                        local_peer_id: self.local_peer_id,
                        caretaker_id,
                        max_peers,
                        peer_storage: std::sync::Arc::clone(&self.peer_storage),
                        crypto_policy,
                        dht_enabled: if bind_ip.is_ipv6() {
                            dht_enabled_ipv6
                        } else {
                            dht_enabled_ipv4
                        },
                    },
                    Arc::clone(&max_peers_state),
                )
            };
            let route = if disable_ipv6 {
                register(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)).await
            } else {
                match register(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)).await {
                    Ok(route) => Ok(route),
                    Err(ipv6_error) => {
                        warn!(
                            error = %ipv6_error,
                            "IPv6 BitTorrent listener unavailable; falling back to IPv4"
                        );
                        register(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)).await
                    }
                }
            }
            .map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: format!("failed to bind BitTorrent peer listener: {error}"),
                })
            })?;
            let (listen_port, incoming_peers, route_handle) = route;
            self.listen_port = listen_port;
            if let Some(registry) = &self.bt_registry
                && let Ok(mut registry) = registry.write()
            {
                registry.set_tcp_port(self.listen_port);
            }
            self.incoming_peers =
                Some(std::sync::Arc::new(tokio::sync::Mutex::new(incoming_peers)));
            self.bt_peer_route = Some(route_handle);
            info!(
                "[BT] Incoming peer route registered on TCP port {}",
                listen_port
            );
        }

        let mut swarm = self
            .initial_peer_swarm
            .take()
            .unwrap_or_else(|| PeerSwarm::new(64));
        swarm.set_local_metadata(Arc::clone(&self.local_metadata));
        tracing::debug!(
            retained_peer_actors = swarm.len(),
            "Preparing payload session with retained torrent swarm"
        );
        let peer_event_tx = swarm
            .event_sender()
            .expect("new torrent swarm must own an event sender");
        let peers = match self
            .discover_peers_with_events(meta, total_size, &network_info_hash, Some(peer_event_tx))
            .await
        {
            Ok(peers) => peers,
            Err(error) => {
                swarm.shutdown_all().await;
                return Err(error);
            }
        };

        // Initialize PEX known peers list from discovered peers for BEP 11 exchange.
        // BEP 0027 (Private Torrent): PEX must be disabled for private torrents
        // because it exchanges peer lists with connected peers, which would leak
        // the swarm membership beyond the tracker-controlled peer set.
        if self.is_private {
            info!("[BT] Private torrent: PEX disabled (BEP 0027)");
        } else {
            info!("[PEX] Outbound peer lists are derived from the live swarm");
        }

        // Initialize web seed manager only when the task explicitly enables
        // the BEP 19 fallback. It reads the current per-file URI queues when
        // requesting each piece so RPC changeUri updates take effect live.
        let web_seed_enabled = self.group.recover().options().bt_enable_web_seed;
        let web_seed_manager = if web_seed_enabled {
            let web_seed_tls = {
                let group = self.group.recover();
                ClientTlsConfig::from_download_options(group.options())
            };
            info!("[BT] Initializing live per-file web-seed fallback");
            Some(Arc::new(
                crate::engine::bittorrent::download::web_seed::WebSeedManager::for_request_group(
                    std::sync::Arc::clone(&self.group),
                    piece_length,
                    total_size,
                    web_seed_tls,
                    self.outbound_network_policy.clone(),
                ),
            ))
        } else {
            None
        };
        // Initialize PEX state only for peers whose BEP 10 handshake advertised
        // ut_pex. Private torrents keep this set empty per BEP 0027.
        let last_pex_send = Instant::now();

        // Download pieces from the connected peers, using web seeds and PEX as configured.
        Ok(TorrentSession {
            initial_peers: peers,
            network_info_hash,
            swarm,
            upload_counter: Arc::new(std::sync::atomic::AtomicU64::new(self.total_uploaded)),
            web_seed_manager,
            last_pex_send,
        })
    }
}
