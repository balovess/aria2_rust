use tracing::{debug, warn};

use crate::engine::bittorrent::peer::message_handler::PeerEvent;
use crate::engine::bittorrent::peer::stats::PeerStats;

use super::super::BtSeedManager;

impl BtSeedManager {
    pub(in crate::engine::bittorrent::download::seed_manager) fn any_peer_choke_state_mismatch(
        &self,
    ) -> bool {
        let desired_unchoked = self
            .swarm
            .iter()
            .filter(|actor| !actor.dead)
            .filter(|actor| !actor.desired_upload_choked())
            .count();
        let has_uninterested_unchoked = self.swarm.iter().any(|actor| {
            !actor.dead && !actor.stats.peer_interested && !actor.desired_upload_choked()
        });
        let has_interested_peer_waiting_for_a_slot = desired_unchoked
            < self.config.max_peers_to_unchoke
            && self.swarm.iter().any(|actor| {
                !actor.dead && actor.stats.peer_interested && actor.desired_upload_choked()
            });

        has_uninterested_unchoked || has_interested_peer_waiting_for_a_slot
    }

    pub(in crate::engine::bittorrent::download::seed_manager) fn apply_peer_event(
        &mut self,
        event: PeerEvent,
    ) {
        self.swarm.apply_event(&event);
        match event {
            PeerEvent::UploadBytes { .. } => {
                self.total_uploaded = self
                    .upload_counter
                    .load(std::sync::atomic::Ordering::Relaxed);
                self.publish_upload_stats();
            }
            PeerEvent::InterestChanged { .. }
            | PeerEvent::AmInterestChanged { .. }
            | PeerEvent::ChokeStateChanged { .. }
            | PeerEvent::PeerChokingChanged { .. }
            | PeerEvent::UploadQueueChanged { .. }
            | PeerEvent::PeerAvailabilityChanged { .. }
            | PeerEvent::PeerAvailabilitySnapshot { .. }
            | PeerEvent::OutstandingDownloadRequests { .. }
            | PeerEvent::AllowedFast { .. }
            | PeerEvent::ExtensionHandshakeReceived { .. }
            | PeerEvent::Disconnected { .. }
            | PeerEvent::GracefulDisconnected { .. }
            | PeerEvent::RequestFailed { .. } => {}
            PeerEvent::PexPeers { peers } => {
                if self
                    .peer_discovery
                    .as_ref()
                    .is_some_and(|discovery| discovery.peer_exchange_enabled())
                {
                    self.store_peer_addresses(
                        peers,
                        crate::request::request_group::BtPeerSource::Pex,
                    );
                }
            }
            PeerEvent::TrackerPeers { peers } => {
                self.store_tracker_peers(
                    peers.into_iter().map(|peer| (peer.ip, peer.port)).collect(),
                );
            }
            PeerEvent::Message { actor_id, .. } => {
                tracing::trace!(
                    actor_id = actor_id.0,
                    "Ignoring block message while seeding"
                );
            }
            PeerEvent::MetadataMessage { .. } => {}
        }
    }

    /// Admit handshaken peers that arrive while the torrent is seeding.
    /// Remove disconnected peers and report whether upstream requires an
    /// immediate choke round for a returned unchoked, interested peer.
    pub(in crate::engine::bittorrent::download::seed_manager) async fn remove_dead_sessions(
        &mut self,
    ) -> bool {
        let needs_choke_round = self
            .swarm
            .iter()
            .any(|actor| actor.dead && actor.stats.peer_interested && !actor.stats.am_choking);
        let removed = self.swarm.remove_dead().await;
        for (_, endpoint) in &removed {
            self.release_peer(*endpoint);
        }
        if !removed.is_empty() {
            debug!("Removed {} dead seeding peer actors", removed.len());
        }
        needs_choke_round
    }

    /// Compute choke decisions from actor-owned stats and apply the results.
    pub(super) fn run_choke_round(&mut self) {
        let mut desired_stats = self
            .swarm
            .iter()
            .map(|actor| {
                let mut stats = actor.stats.clone();
                stats.am_choking = actor.desired_upload_choked();
                stats
            })
            .collect::<Vec<_>>();
        let mut peers_mut: Vec<&mut PeerStats> = desired_stats.iter_mut().collect();
        self.seeder_choke.execute_choke(&mut peers_mut[..]);

        let desired_choking = desired_stats
            .into_iter()
            .map(|stats| stats.am_choking)
            .collect::<Vec<_>>();
        for (actor, desired) in self.swarm.iter().zip(desired_choking) {
            if desired == actor.desired_upload_choked() {
                continue;
            }
            if !self.swarm.set_upload_choked(actor.actor_id, desired) {
                warn!(
                    actor_id = actor.actor_id.0,
                    "Peer actor was unavailable while applying choke decision"
                );
            }
        }
    }
}
