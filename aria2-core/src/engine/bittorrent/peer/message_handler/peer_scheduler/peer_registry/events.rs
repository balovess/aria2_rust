use std::net::SocketAddr;
use std::time::Instant;

use super::super::PeerEvent;
use super::PeerSwarm;
use super::entry::update_peer_availability;

impl PeerSwarm {
    fn remember_known_seeder(&mut self, endpoint: SocketAddr) {
        if !self.known_seeders.insert(endpoint) {
            return;
        }
        self.known_seeder_order.push_back(endpoint);
        while self.known_seeder_order.len() > super::MAX_KNOWN_SEEDER_ENDPOINTS {
            if let Some(expired) = self.known_seeder_order.pop_front() {
                self.known_seeders.remove(&expired);
            }
        }
    }

    /// Apply one consumed I/O event to the registry-owned peer snapshot.
    pub(crate) fn apply_event(&mut self, event: &PeerEvent) {
        if let PeerEvent::UploadBytes {
            bytes, recorded_at, ..
        } = event
        {
            self.upload_rate.record(*bytes, *recorded_at);
        }
        match event {
            PeerEvent::InterestChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::AmInterestChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::ChokeStateChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::UploadBytes {
                actor_id, snapshot, ..
            }
            | PeerEvent::UploadQueueChanged {
                actor_id, snapshot, ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats = (**snapshot).clone();
                }
            }
            PeerEvent::Message {
                actor_id,
                stats: Some(snapshot),
                ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats = (**snapshot).clone();
                }
            }
            PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index,
                has_piece,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    update_peer_availability(actor, *piece_index, *has_piece);
                }
            }
            PeerEvent::PeerAvailabilitySnapshot {
                actor_id,
                bitfield,
                seeder,
            } => {
                if *seeder
                    && let Some((endpoint, advertised_endpoint)) = self
                        .actor(*actor_id)
                        .map(|actor| (actor.endpoint, actor.advertised_endpoint))
                {
                    self.remember_known_seeder(endpoint);
                    if let Some(advertised_endpoint) = advertised_endpoint {
                        self.remember_known_seeder(advertised_endpoint);
                    }
                }
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.bitfield.clone_from(bitfield);
                    actor.has_bitfield = true;
                    actor.seeder = *seeder;
                }
            }
            PeerEvent::OutstandingDownloadRequests { actor_id, count } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor
                        .pending_download_requests
                        .store(*count, std::sync::atomic::Ordering::Relaxed);
                }
            }
            PeerEvent::PeerChokingChanged {
                actor_id,
                peer_choking,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats.peer_choking = *peer_choking;
                }
            }
            PeerEvent::AllowedFast {
                actor_id,
                piece_index,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.peer_allowed_fast.insert(*piece_index);
                }
            }
            PeerEvent::ExtensionHandshakeReceived {
                actor_id,
                ut_pex_id,
                remote_listen_port,
                ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.ut_pex_id = *ut_pex_id;
                    if let Some(port) = remote_listen_port {
                        actor.advertised_endpoint =
                            Some(SocketAddr::new(actor.endpoint.ip(), *port));
                        actor.incoming = false;
                    }
                }
            }
            PeerEvent::Disconnected { actor_id } => {
                self.mark_dead(*actor_id);
            }
            PeerEvent::GracefulDisconnected { actor_id } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.graceful_disconnected_at = Some(Instant::now());
                }
                self.mark_dead(*actor_id);
            }
            PeerEvent::RequestFailed { .. } => {}
            PeerEvent::Message { stats: None, .. }
            | PeerEvent::MetadataMessage { .. }
            | PeerEvent::PexPeers { .. }
            | PeerEvent::TrackerPeers { .. } => {}
        }
        let publishes_peer_state = matches!(
            event,
            PeerEvent::InterestChanged { .. }
                | PeerEvent::AmInterestChanged { .. }
                | PeerEvent::ChokeStateChanged { .. }
                | PeerEvent::PeerChokingChanged { .. }
                | PeerEvent::PeerAvailabilityChanged { .. }
                | PeerEvent::PeerAvailabilitySnapshot { .. }
                | PeerEvent::ExtensionHandshakeReceived {
                    remote_listen_port: Some(_),
                    ..
                }
        );
        let publishes_peer_stats = matches!(
            event,
            PeerEvent::UploadBytes { .. }
                | PeerEvent::UploadQueueChanged { .. }
                | PeerEvent::OutstandingDownloadRequests { .. }
                | PeerEvent::Message { stats: Some(_), .. }
        );
        let now = Instant::now();
        let stats_snapshot_due = publishes_peer_stats
            && self.last_stats_snapshot_publish.is_none_or(|last| {
                now.saturating_duration_since(last) >= super::PEER_STATS_SNAPSHOT_MIN_INTERVAL
            });
        if publishes_peer_state {
            self.publish_peer_snapshots();
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
        }
        if stats_snapshot_due {
            if !publishes_peer_state {
                self.publish_peer_snapshots();
            }
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
        } else if publishes_peer_stats {
            self.stats_snapshot_dirty = true;
        }
    }

    pub(super) fn pending_stats_snapshot_deadline(&self) -> Option<Instant> {
        self.stats_snapshot_dirty.then(|| {
            self.last_stats_snapshot_publish
                .map_or_else(Instant::now, |last| {
                    last + super::PEER_STATS_SNAPSHOT_MIN_INTERVAL
                })
        })
    }

    pub(super) fn publish_pending_stats_snapshot(&mut self) {
        let now = Instant::now();
        if self.stats_snapshot_dirty
            && self.last_stats_snapshot_publish.is_none_or(|last| {
                now.saturating_duration_since(last) >= super::PEER_STATS_SNAPSHOT_MIN_INTERVAL
            })
        {
            self.publish_peer_snapshots();
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
        }
    }
}
