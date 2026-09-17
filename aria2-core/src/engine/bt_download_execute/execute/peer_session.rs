use std::collections::HashSet;
use std::time::Instant;

use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_download_execute::types::PeerKey;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, FatalError};
use crate::http::client_identity::ClientTlsConfig;
use crate::util::rwlock_ext::RwLockRecover;

use super::parse_listen_ports;

pub(super) struct PeerSession {
    pub(super) active_connections: Vec<BtPeerConn>,
    pub(super) web_seed_manager: Option<crate::engine::bt_web_seed::WebSeedManager>,
    pub(super) pex_enabled_peers: HashSet<PeerKey>,
    pub(super) last_pex_send: Instant,
}

impl BtDownloadCommand {
    pub(super) async fn prepare_peer_session(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        network_info_hash: [u8; 20],
    ) -> crate::error::Result<PeerSession> {
        // BtSetup/PeerListenCommand counterpart: register this torrent on the
        // engine-owned listener before discovery so one socket can serve all
        // active torrents and route by info-hash.
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
            let register = |bind_ip: std::net::IpAddr| {
                listener_manager.register(crate::engine::bt_peer_listener::BtPeerRouteConfig {
                    bind_ip,
                    ports: listen_ports.clone(),
                    info_hash: network_info_hash,
                    info_hash_v2: meta.info_hash_v2,
                    local_peer_id: self.local_peer_id,
                    caretaker_id,
                    max_peers,
                    peer_storage: std::sync::Arc::clone(&self.peer_storage),
                    crypto_policy,
                })
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
            self.incoming_peers = Some(incoming_peers);
            self.bt_peer_route = Some(route_handle);
            info!(
                "[BT] Incoming peer route registered on TCP port {}",
                listen_port
            );
        }

        let peer_addrs = self
            .discover_peers(meta, total_size, &network_info_hash)
            .await?;

        let mut active_connections = if peer_addrs.is_empty() {
            Vec::new()
        } else {
            self.connect_to_peers(
                &peer_addrs,
                &network_info_hash,
                meta.info_hash_v2,
                num_pieces,
                piece_length,
                total_size,
            )
            .await?
        };

        // The initial DHT lookup is complete once discovery and the first
        // PeerStorage admission have finished. Record the same count the
        // original DHTGetPeersCommand uses for retry decisions.
        if self.dht_engine.is_some() {
            self.dht_periodic_lookup
                .set_peer_limits(self.bt_runtime.min_peers(), self.bt_runtime.max_peers());
            self.dht_periodic_lookup
                .record_lookup_completed(self.tracked_peer_count());
        }

        // Initialize PEX known peers list from discovered peers for BEP 11 exchange.
        // BEP 0027 (Private Torrent): PEX must be disabled for private torrents
        // because it exchanges peer lists with connected peers, which would leak
        // the swarm membership beyond the tracker-controlled peer set.
        if self.is_private {
            info!("[BT] Private torrent: PEX disabled (BEP 0027)");
        } else {
            let pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = peer_addrs
                .iter()
                .map(|pa| {
                    aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&pa.ip, pa.port)
                })
                .collect();
            self.set_pex_known_peers(pex_peers);
            info!(
                "[PEX] Initialized with {} known peers from tracker/DHT",
                self.pex_known_peers.len()
            );
        }

        // Initialize web seed manager only when the task explicitly enables
        // the BEP 19 fallback. The torrent's url-list is metadata, not an
        // instruction to bypass --bt-enable-web-seed=false.
        let web_seed_enabled = self.group.recover().options().bt_enable_web_seed;
        let web_seed_urls = self.configured_web_seed_urls();
        let web_seed_manager = if web_seed_enabled && !web_seed_urls.is_empty() {
            info!(
                "[BT] Initializing web seed manager with {} URL(s)",
                web_seed_urls.len()
            );
            let web_seed_tls = {
                let group = self.group.recover();
                ClientTlsConfig::from_download_options(group.options())
            };
            Some(
                crate::engine::bt_web_seed::WebSeedManager::new_with_tls(
                    web_seed_urls,
                    piece_length,
                    total_size,
                    &web_seed_tls,
                )
                .map_err(|error| {
                    Aria2Error::Fatal(FatalError::Config(format!(
                        "Web-seed HTTP client configuration failed: {error}"
                    )))
                })?,
            )
        } else {
            None
        };

        // Initialize PEX state only for peers whose BEP 10 handshake advertised
        // ut_pex. Private torrents keep this set empty per BEP 0027.
        let mut pex_enabled_peers: HashSet<PeerKey> = HashSet::new();
        let last_pex_send = Instant::now();

        if self.peer_exchange_enabled() {
            // PEX is enabled only after the remote BEP 10 handshake advertises
            // ut_pex. Each peer has an independent extension-ID namespace.
            for conn in active_connections.iter() {
                if conn.peer_extension_id("ut_pex").is_some()
                    && let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port)
                {
                    pex_enabled_peers.insert(peer_key);
                }
            }
            info!(
                "[PEX] Initialized PEX tracking for {} negotiated peers",
                pex_enabled_peers.len()
            );
        }

        // BEP 6 (Fast Extension): Send initial AllowedFast messages to
        // peers that support fast extension. The fast-set is computed from
        // the peer's IP address (see compute_fast_set in fast_set.rs).
        // Also, for peers that support fast extension, we can send HaveAll
        // or HaveNone instead of a full Bitfield when appropriate.
        for (idx, conn) in active_connections.iter_mut().enumerate() {
            if conn.is_fast_extension_enabled() {
                let mut sent = HashSet::new();
                match Self::send_allowed_fast_for_torrent(
                    conn,
                    num_pieces,
                    &network_info_hash,
                    &mut sent,
                )
                .await
                {
                    Ok(count) if count > 0 => {
                        if let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                            self.allowed_fast_sent_peers.insert(peer_key, sent);
                        }
                        debug!(
                            "[BEP6] Sent {} AllowedFast pieces to peer {} ({})",
                            count, idx, conn.ip_addr
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("[BEP6] Failed to flush AllowedFast to peer {}: {}", idx, e);
                    }
                }
            }
        }

        // Admit handshaken incoming peers before the first piece cycle. Later
        // cycles drain the receiver below, preserving PeerListenCommand's
        // long-lived listener semantics.
        self.drain_incoming_peers(&mut active_connections, piece_length, total_size);

        // Download pieces from the connected peers, using web seeds and PEX as configured.
        Ok(PeerSession {
            active_connections,
            web_seed_manager,
            pex_enabled_peers,
            last_pex_send,
        })
    }
}
