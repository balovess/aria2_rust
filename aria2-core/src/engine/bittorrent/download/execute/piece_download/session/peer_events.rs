use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::download::execute::types::PeerKey;
use crate::engine::bittorrent::peer::choking_algorithm::PeerIdentity;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::message_handler::{PeerCommand, PeerEvent};
use crate::request::request_group::BtPeerSource;
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

use super::PieceDownloadSession;
use super::peer_dials::{PeerDialConfig, PeerDialQueue};

impl PieceDownloadSession<'_> {
    pub(super) fn apply_swarm_peer_event(&mut self, event: &PeerEvent) -> bool {
        match event {
            PeerEvent::InterestChanged { snapshot, .. }
            | PeerEvent::ChokeStateChanged { snapshot, .. }
            | PeerEvent::UploadBytes { snapshot, .. }
            | PeerEvent::UploadQueueChanged { snapshot, .. } => {
                self.command.track_peer_for_upload_choking(snapshot);
                matches!(event, PeerEvent::InterestChanged { .. })
            }
            PeerEvent::AmInterestChanged { .. } => false,
            PeerEvent::PeerChokingChanged { actor_id, .. } => {
                if let Some(actor) = self.swarm.actor(*actor_id) {
                    self.command.track_peer_for_upload_choking(&actor.stats);
                }
                false
            }
            PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index,
                has_piece,
            } => {
                if let Some(actor) = self.swarm.actor(*actor_id) {
                    let peer = actor.endpoint.to_string();
                    let availability_changed =
                        self.peer_tracker
                            .update_peer_piece(&peer, *piece_index, *has_piece);
                    if availability_changed {
                        self.peer_last_data_time
                            .insert(PeerKey::new(actor.endpoint), Instant::now());
                        self.piece_picker
                            .set_frequencies_from_peers(&self.peer_tracker.piece_frequencies());
                    }
                }
                false
            }
            PeerEvent::PeerAvailabilitySnapshot {
                actor_id,
                bitfield,
                seeder,
            } => {
                if let Some(actor) = self.swarm.actor(*actor_id) {
                    let peer = actor.endpoint.to_string();
                    let bitfield = if *seeder {
                        vec![0xff; (self.num_pieces as usize).div_ceil(8)]
                    } else {
                        bitfield.clone()
                    };
                    self.peer_tracker.update_peer_bitfield(&peer, &bitfield);
                    self.piece_picker
                        .set_frequencies_from_peers(&self.peer_tracker.piece_frequencies());
                    self.peer_last_data_time
                        .insert(PeerKey::new(actor.endpoint), Instant::now());
                }
                false
            }
            PeerEvent::PexPeers { peers } => {
                self.pending_pex_peers.extend(peers.iter().cloned());
                false
            }
            PeerEvent::TrackerPeers { peers } => {
                self.pending_tracker_peers.extend(peers.iter().cloned());
                false
            }
            PeerEvent::AllowedFast { .. } => false,
            PeerEvent::ExtensionHandshakeReceived { .. } => false,
            PeerEvent::MetadataMessage { .. } => false,
            PeerEvent::Message {
                actor_id,
                message: aria2_protocol::bittorrent::message::types::BtMessage::Piece { .. },
                stats: Some(snapshot),
                ..
            } => {
                self.command.track_peer_for_upload_choking(snapshot);
                if let Some(actor) = self.swarm.actor(*actor_id) {
                    self.peer_last_data_time
                        .insert(PeerKey::new(actor.endpoint), Instant::now());
                }
                false
            }
            PeerEvent::Disconnected { .. }
            | PeerEvent::GracefulDisconnected { .. }
            | PeerEvent::OutstandingDownloadRequests { .. }
            | PeerEvent::RequestFailed { .. }
            | PeerEvent::Message { .. } => false,
        }
    }

    pub(super) async fn remove_dead_swarm_peers(&mut self) -> bool {
        let dead = self
            .swarm
            .iter()
            .filter(|actor| actor.dead)
            .map(|actor| {
                (
                    PeerIdentity::from(&actor.stats),
                    actor.endpoint,
                    PeerKey::new(actor.endpoint),
                )
            })
            .collect::<Vec<_>>();
        if dead.is_empty() {
            return false;
        }

        let released_upload_slot = self.command.choking_algo.as_ref().is_some_and(|algo| {
            dead.iter().any(|(identity, _, _)| {
                algo.peers().iter().any(|peer| {
                    PeerIdentity::from(peer) == *identity
                        && peer.peer_interested
                        && !peer.am_choking
                })
            })
        });

        let identities = dead
            .iter()
            .map(|(identity, _, _)| *identity)
            .collect::<Vec<_>>();
        if let Some(choking) = self.command.choking_algo.as_mut() {
            choking.remove_peers_by_identity(&identities);
        }
        let peer_keys = dead.iter().map(|(_, _, key)| *key).collect::<Vec<_>>();
        self.endgame_state.remove_peers(&peer_keys);
        for (_, endpoint, peer_key) in &dead {
            self.peer_tracker.remove_peer(&endpoint.to_string());
            self.peer_last_data_time.remove(peer_key);
        }
        self.piece_picker
            .set_frequencies_from_peers(&self.peer_tracker.piece_frequencies());

        let removed = self.swarm.remove_dead().await;
        let mut peer_storage = self
            .command
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, endpoint) in removed {
            peer_storage.return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
        }
        released_upload_slot
    }

    pub(super) async fn handle_peer_wait_event(
        &mut self,
        event: super::super::peer_events::PeerWaitEvent,
    ) {
        let interest_changed = match &event {
            super::super::peer_events::PeerWaitEvent::Actor(actor_event) => {
                self.apply_swarm_peer_event(actor_event)
            }
            _ => false,
        };
        if let super::super::peer_events::PeerWaitEvent::Incoming(incoming) = event {
            let context = self.peer_actor_admission_context();
            self.command
                .admit_incoming_peer_to_swarm(self.swarm, *incoming, &context);
            self.announce_available_pieces().await;
        }
        let released_upload_slot = self.remove_dead_swarm_peers().await;
        if interest_changed || released_upload_slot {
            self.apply_upload_choke_round();
        }
    }

    pub(super) fn queue_discovered_swarm_peers(
        &mut self,
        peers: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        source: BtPeerSource,
        peer_dials: &mut PeerDialQueue,
        dial_config: &PeerDialConfig,
    ) -> usize {
        self.command
            .peer_coordinator
            .set_max_peers(self.command.bt_runtime.max_peers());
        let active_peers = self
            .swarm
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| (actor.endpoint.ip().to_string(), actor.endpoint.port()))
            .collect::<std::collections::HashSet<_>>();
        let max_new_connections = self
            .command
            .peer_coordinator
            .available_slots(active_peers.len());
        if max_new_connections == 0 {
            return 0;
        }
        let candidates =
            self.command
                .peer_coordinator
                .select_candidates(peers, &active_peers, |ip| {
                    self.command.is_peer_temporarily_rejected(ip)
                });
        if candidates.is_empty() {
            return 0;
        }
        let queued = peer_dials.enqueue(candidates, source);
        self.start_next_peer_dial_batch(peer_dials, dial_config);
        queued
    }

    pub(super) fn start_next_peer_dial_batch(
        &mut self,
        peer_dials: &mut PeerDialQueue,
        dial_config: &PeerDialConfig,
    ) {
        self.command
            .peer_coordinator
            .set_max_peers(self.command.bt_runtime.max_peers());
        let active_count = self.swarm.iter().filter(|actor| !actor.dead).count();
        let available_slots = self.command.peer_coordinator.available_slots(active_count);
        peer_dials.start_next(available_slots, dial_config);
    }

    pub(super) async fn handle_peer_dial_batch_result(
        &mut self,
        result: std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>,
    ) {
        let mut connections = match result {
            Ok(connections) => connections,
            Err(error) => {
                warn!(%error, "Peer handshake batch task failed");
                return;
            }
        };
        for connection in &mut connections {
            self.command.apply_peer_exchange_policy(connection);
        }
        let admitted = self.admit_connected_peers(connections);
        if admitted > 0 {
            self.announce_available_pieces().await;
            let group = self.command.group.recover();
            super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
            info!(admitted, "Admitted discovered peers into the active Swarm");
        }
    }

    pub(super) fn apply_upload_choke_round(&mut self) {
        self.command.apply_upload_choke_round_swarm(self.swarm);
    }

    fn admit_connected_peers(&mut self, new_connections: Vec<BtPeerConn>) -> usize {
        let max_peers = self.command.group.recover().options().bt_max_peers;
        let caretaker_id = self.command.group.recover().gid().value();
        let mut seen_endpoints = std::collections::HashSet::with_capacity(new_connections.len());
        let provider = std::sync::Arc::clone(&self.upload_provider);
        let mut admitted = 0;

        for mut connection in new_connections {
            if max_peers > 0 && self.swarm.len() >= max_peers {
                break;
            }
            let Some(endpoint) = connection.remote_endpoint() else {
                tracing::debug!("[BT] Dropping new peer without a remote endpoint");
                continue;
            };
            if endpoint.ip().is_unspecified()
                || endpoint.port() == 0
                || !seen_endpoints.insert(endpoint)
                || self.swarm.has_endpoint(endpoint)
                || connection
                    .remote_peer_id()
                    .is_some_and(|peer_id| self.swarm.has_peer_id(peer_id))
                || connection.remote_peer_id() == Some(self.command.local_peer_id)
            {
                tracing::debug!(peer = %endpoint, "Dropping duplicate or invalid discovered peer");
                continue;
            }

            let entry = crate::engine::bittorrent::peer::storage::PeerEntry::new(
                endpoint.ip().to_string(),
                endpoint.port(),
            );
            let checked_out = self
                .command
                .peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .add_and_checkout_peer(entry, caretaker_id)
                .is_some();
            if !checked_out {
                continue;
            }

            self.command.configure_upload_connection(
                &mut connection,
                self.piece_length,
                self.num_pieces,
            );
            connection.set_upload_counter(Arc::clone(&self.upload_counter));
            connection.set_upload_progress(Arc::clone(&self.command.progress));
            let peer_key = PeerKey::new(endpoint);
            let stats = connection.stats.clone();

            if self
                .swarm
                .spawn_peer(
                    connection,
                    self.command.dht_engines.for_peer(endpoint),
                    Arc::clone(&provider),
                )
                .is_err()
            {
                self.command.release_peer_endpoint(endpoint);
                continue;
            }

            self.command
                .peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_peer_active(&endpoint.ip().to_string(), endpoint.port(), true);
            self.peer_last_data_time.insert(peer_key, Instant::now());
            self.command.track_peer_for_upload_choking(&stats);
            admitted += 1;
        }

        if admitted > 0 {
            tracing::debug!(admitted, "[BT] Admitted discovered peers to the swarm");
            self.command.bt_runtime.set_connections(self.swarm.len());
            self.command
                .group
                .recover()
                .set_bt_connection_count(self.swarm.len());
        }
        admitted
    }

    pub(super) async fn announce_available_pieces(&mut self) {
        let actor_ids = self
            .swarm
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| actor.actor_id)
            .collect::<Vec<_>>();
        for actor_id in actor_ids {
            if self
                .swarm
                .send_to(actor_id, PeerCommand::AnnounceAvailability)
                .await
                .is_err()
            {
                self.swarm.mark_dead(actor_id);
            }
        }
    }

    pub(super) fn check_and_mark_swarm_peers_snubbed(&mut self) {
        let timeout = self
            .command
            .group
            .recover()
            .options()
            .bt_snubbed_timeout
            .unwrap_or(60);
        let now = Instant::now();
        if !self.swarm.iter().filter(|actor| !actor.dead).any(|actor| {
            actor
                .stats
                .next_snubbed_deadline(timeout)
                .is_some_and(|deadline| deadline <= now)
        }) {
            return;
        }
        for actor in self.swarm.iter_mut().filter(|actor| !actor.dead) {
            if actor.stats.check_snubbed_at(timeout, now) {
                debug!(peer = %actor.endpoint, timeout, "Marked peer actor as snubbed");
            }
        }
    }

    pub(super) fn next_snub_check_deadline(&self) -> Option<Instant> {
        let timeout = self
            .command
            .group
            .recover()
            .options()
            .bt_snubbed_timeout
            .unwrap_or(60);
        self.swarm
            .iter()
            .filter(|actor| !actor.dead)
            .filter_map(|actor| actor.stats.next_snubbed_deadline(timeout))
            .min()
    }
}
