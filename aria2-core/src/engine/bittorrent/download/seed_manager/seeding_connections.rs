use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::engine::bittorrent::peer::interaction::{BtPeerInteraction, PeerConnectionResult};
use crate::engine::bittorrent::peer::message_handler::PeerCommand;
use crate::engine::bittorrent::peer::storage::PeerEntry;
use crate::request::request_group::BtPeerSource;
use crate::util::rwlock_ext::RwLockRecover;

use super::{BtSeedManager, SeedPeerConnectionAttempt};

const MAX_SEED_CONNECTION_BATCH: usize = 32;

impl BtSeedManager {
    pub(super) fn store_tracker_peers(&mut self, peers: Vec<(String, u16)>) {
        if peers.is_empty() {
            return;
        }
        self.store_peer_addresses(
            peers
                .into_iter()
                .map(|(ip, port)| {
                    aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port)
                })
                .collect(),
            BtPeerSource::Tracker,
        );
    }

    pub(super) fn store_peer_addresses(
        &mut self,
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
        source: BtPeerSource,
    ) {
        let Some(peer_storage) = self.peer_storage.as_ref() else {
            return;
        };
        let peers = peers
            .into_iter()
            .filter_map(|peer| {
                let endpoint = peer.to_socket_addr().ok()?;
                Some((peer.ip, peer.port, endpoint))
            })
            .filter(|(_, _, endpoint)| !self.swarm.is_known_seeder(*endpoint))
            .collect::<Vec<_>>();
        if peers.is_empty() {
            return;
        }
        let mut storage = peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = storage.count_all_peers();
        storage.add_peers(
            peers
                .iter()
                .map(|(ip, port, _)| PeerEntry::new(ip.clone(), *port))
                .collect(),
        );
        let added = storage.count_all_peers().saturating_sub(before);
        if added > 0 {
            debug!(
                added,
                "Added discovered BitTorrent peers to seeding storage"
            );
        }
        for (ip, port, endpoint) in &peers {
            if storage
                .get_peer(ip, *port)
                .is_some_and(|peer| !peer.is_active)
            {
                self.peer_sources.entry(*endpoint).or_insert(source);
            }
        }
        self.peer_sources.retain(|endpoint, _| {
            storage
                .get_peer(&endpoint.ip().to_string(), endpoint.port())
                .is_some_and(|peer| !peer.is_active)
        });
    }

    pub(super) async fn collect_periodic_dht_peers(&mut self) {
        let (peers, lookup_completion_pending) = {
            let Some(discovery) = self.peer_discovery.as_mut() else {
                return;
            };
            if discovery.dht_engines.is_empty() {
                return;
            }

            let max_peers = discovery.group.recover().options().bt_max_peers;
            let min_peers = if max_peers == 0 {
                0
            } else {
                (max_peers * 4 / 5).max(1)
            };
            discovery.dht_lookup.set_peer_limits(min_peers, max_peers);
            let mut peers = Vec::new();
            crate::engine::bittorrent::download::execute::check_periodic_dht_lookup(
                &mut discovery.dht_lookup,
                &discovery.dht_engines,
                &self.info_hash,
                discovery.listen_port,
                self.swarm.len(),
                &mut peers,
            )
            .await;
            (peers, discovery.dht_lookup.is_lookup_completion_pending())
        };
        self.store_peer_addresses(peers, BtPeerSource::Dht);

        if lookup_completion_pending {
            let tracked_peer_count = self
                .peer_storage
                .as_ref()
                .map(|peer_storage| {
                    peer_storage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .count_all_peers()
                })
                .unwrap_or(self.swarm.len());
            if let Some(discovery) = self.peer_discovery.as_mut() {
                discovery.dht_lookup.on_lookup_completed(tracked_peer_count);
            }
        }
    }

    pub(super) fn start_peer_connection_attempt(&mut self) {
        if self.pending_peer_connection.is_some() {
            return;
        }
        let Some(discovery) = self.peer_discovery.as_ref() else {
            return;
        };
        let Some(peer_storage) = self.peer_storage.as_ref() else {
            return;
        };

        let (max_peers, caretaker_id, max_upload_limit) = {
            let group = discovery.group.recover();
            (
                group.options().bt_max_peers,
                group.gid().value(),
                group.options().max_upload_limit,
            )
        };
        if max_upload_limit.is_some_and(|limit| {
            limit > 0 && self.current_upload_speed().saturating_mul(10) >= limit.saturating_mul(8)
        }) {
            return;
        }

        let current_connections = self.swarm.len() + self.pending_connections.len();
        let available_slots = if max_peers == 0 {
            MAX_SEED_CONNECTION_BATCH
        } else {
            max_peers.saturating_sub(current_connections)
        }
        .min(MAX_SEED_CONNECTION_BATCH);
        if available_slots == 0 {
            return;
        }

        let checked_out = {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (0..available_slots)
                .filter_map(|_| storage.checkout_peer(caretaker_id))
                .collect::<Vec<_>>()
        };
        if checked_out.is_empty() {
            return;
        }

        let addresses = checked_out
            .iter()
            .map(|peer| {
                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&peer.ip, peer.port)
            })
            .collect::<Vec<_>>();
        let info_hash = self.info_hash;
        let connection_options = discovery.connection_options.clone();
        let utp_socket = discovery.utp_socket.clone();
        let policy = Arc::clone(&discovery.outbound_network_policy);
        let piece_length = self.piece_provider.piece_length();
        let num_pieces = self.piece_provider.num_pieces();
        let total_size = discovery.total_size;
        let task = tokio::spawn(async move {
            BtPeerInteraction::connect_to_peers(
                &addresses,
                &info_hash,
                num_pieces,
                piece_length,
                total_size,
                &connection_options,
                utp_socket,
                &policy,
            )
            .await
        });
        info!(
            count = checked_out.len(),
            "Starting bounded seeding peer connection batch"
        );
        self.pending_peer_connection = Some(SeedPeerConnectionAttempt { task, checked_out });
    }

    pub(super) fn finish_peer_connection_attempt(
        &mut self,
        result: std::result::Result<
            crate::error::Result<PeerConnectionResult>,
            tokio::task::JoinError,
        >,
    ) {
        let Some(attempt) = self.pending_peer_connection.take() else {
            return;
        };
        let mut checked_out = attempt
            .checked_out
            .into_iter()
            .map(|peer| ((peer.ip.to_string(), peer.port), peer))
            .collect::<HashMap<_, _>>();
        let connection_result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                warn!(%error, "Seeding peer connection batch failed");
                self.return_checked_out(checked_out.into_values());
                return;
            }
            Err(error) => {
                warn!(%error, "Seeding peer connection task failed");
                self.return_checked_out(checked_out.into_values());
                return;
            }
        };

        let mut peer_ids = self
            .swarm
            .iter()
            .map(|actor| actor.stats.peer_id)
            .collect::<HashSet<_>>();
        peer_ids.insert(self.peer_id);
        let dht_engines = self
            .peer_discovery
            .as_ref()
            .map(|discovery| discovery.dht_engines.clone());
        let pex_enabled = self
            .peer_discovery
            .as_ref()
            .is_some_and(|discovery| discovery.enable_peer_exchange);

        for mut connection in connection_result.connections {
            let Some(endpoint) = connection.remote_endpoint() else {
                drop(connection);
                continue;
            };
            let key = (endpoint.ip().to_string(), endpoint.port());
            if !checked_out.contains_key(&key)
                || self.swarm.has_endpoint(endpoint)
                || connection
                    .remote_peer_id()
                    .is_some_and(|peer_id| !peer_ids.insert(peer_id))
            {
                drop(connection);
                continue;
            }

            let source = self
                .peer_sources
                .get(&endpoint)
                .copied()
                .unwrap_or(BtPeerSource::Unknown);
            connection.set_source(source);
            connection.set_pex_enabled(pex_enabled);
            connection.configure_upload_with_auto_unchoke(
                &self.config,
                self.torrent_upload_limiter.clone(),
                self.piece_provider.num_pieces(),
                self.piece_provider.piece_length(),
                false,
            );
            connection.set_upload_counter(Arc::clone(&self.upload_counter));
            connection.stats.am_choking = true;

            let actor_id = match self.swarm.spawn_peer(
                connection,
                dht_engines
                    .as_ref()
                    .and_then(|engines| engines.for_peer(endpoint)),
                Arc::clone(&self.piece_provider),
            ) {
                Ok(actor_id) => actor_id,
                Err(connection) => {
                    drop(connection);
                    continue;
                }
            };

            if let Some(peer) = checked_out.remove(&key) {
                self.peer_sources.remove(&endpoint);
                if let Some(peer_storage) = self.peer_storage.as_ref() {
                    peer_storage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_peer_active(&peer.ip, peer.port, true);
                }
            }
            if let Some(actor) = self.swarm.actor_mut(actor_id)
                && let Err(error) = actor.try_send(PeerCommand::AnnounceAvailability)
            {
                actor.dead = true;
                warn!(%endpoint, %error, "Failed to announce pieces to new seeding peer");
            }
            info!(%endpoint, "Connected a discovered peer during seeding");
        }

        self.return_checked_out(checked_out.into_values());
        self.publish_connection_state();
    }

    fn return_checked_out(&mut self, peers: impl IntoIterator<Item = PeerEntry>) {
        let Some(peer_storage) = self.peer_storage.as_ref() else {
            return;
        };
        let mut storage = peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for peer in peers {
            if let Ok(ip) = peer.ip.parse::<std::net::IpAddr>() {
                self.peer_sources
                    .remove(&std::net::SocketAddr::new(ip, peer.port));
            }
            storage.return_peer(&peer);
        }
    }

    pub(super) async fn cancel_peer_connection_attempt(&mut self) {
        let Some(attempt) = self.pending_peer_connection.take() else {
            return;
        };
        attempt.task.abort();
        let _ = attempt.task.await;
        self.return_checked_out(attempt.checked_out);
    }
}
