use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::peer_stats::PeerStats;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS};
use crate::engine::bt_message_handler::{PeerCommand, PeerEvent};

enum SeedWaitEvent {
    Incoming(crate::engine::bt_peer_listener::IncomingPeer),
    PeerEvent(PeerEvent),
    Wake,
}

impl BtSeedManager {
    // -----------------------------------------------------------------------
    // Main seeding loop
    // -----------------------------------------------------------------------

    /// Run the main seeding loop until exit conditions are met or cancelled.
    ///
    /// Mirrors C++ `SeedCheckCommand::execute()` while waiting on peer-worker,
    /// listener, cancellation, and deadline events instead of scanning on a
    /// fixed interval.
    pub async fn run_seeding_loop(&mut self) -> crate::error::Result<()> {
        info!(
            info_hash = ?self.info_hash,
            "Seeding loop started (ratio={:?}, time={:?}, peers={})",
            self.exit_condition.seed_ratio,
            self.exit_condition.seed_time,
            self.upload_sessions.len()
        );

        self.remove_dead_sessions().await;
        self.start_seed_peer_actors();
        for actor in self.swarm.iter_mut() {
            if let Err(error) = actor.try_send(PeerCommand::AnnounceAvailability) {
                actor.dead = true;
                warn!(%error, actor_id = actor.actor_id.0, "Failed to queue seed availability announcement");
            }
        }
        self.publish_connection_state();
        self.publish_upload_stats();

        loop {
            // -- Cancellation check (non-blocking) ----------------------------
            if self.cancel_token.is_cancelled() {
                info!("Seeding loop cancelled via cancellation token");
                break;
            }

            // -- Exit condition check ----------------------------------------
            if self.should_stop_seeding() {
                self.halt_requested = true;
                info!(
                    "Seed exit conditions met (uploaded={}, downloaded={}, duration={:?})",
                    self.total_uploaded,
                    self.total_downloaded,
                    self.seeding_duration()
                );
                break;
            }

            // -- Tracker re-announce (mirrors C++ SeedCheckCommand keeping the
            // swarm informed via BtAnnounce; the state machine throttles by
            // the tracker-provided interval) ----------------------------------
            if let Some(announcer) = self.announcer.as_mut()
                && announcer.is_default_announce_ready()
                && let Some(result) = announcer
                    .announce(
                        &self.info_hash,
                        &self.peer_id,
                        self.total_downloaded,
                        0,
                        self.total_uploaded,
                    )
                    .await
            {
                debug!(
                    "[Seed] Re-announced to {} ({:?} seeders, {:?} leechers)",
                    result.tracker_url, result.seeders, result.leechers
                );
            }

            // The listener remains active after the payload is complete. This
            // is the Rust equivalent of PeerListenCommand continuing beside
            // SeedCheckCommand, including when no peer was connected at the
            // instant the download finished.
            self.drain_incoming_peers().await;

            // -- Process state changes observed since the last event ---------
            let removed_peer_needs_choke_round = self.remove_dead_sessions().await;
            self.sync_sessions_to_stats();
            self.publish_connection_state();
            let peer_choke_state_needs_decision = self.any_peer_choke_state_mismatch();
            if removed_peer_needs_choke_round
                || self.last_choke_time.elapsed().as_secs() >= CHOKE_ROUND_INTERVAL_SECS
                || peer_choke_state_needs_decision
            {
                self.run_choke_round().await;
                self.last_choke_time = Instant::now();
            }

            // Park until a socket message, an incoming peer, cancellation, or
            // a protocol/seed deadline wakes the manager. There is no fixed
            // interval scan when the swarm is idle.
            match self
                .wait_for_seed_event(self.next_seed_event_deadline())
                .await
            {
                SeedWaitEvent::Incoming(incoming) => {
                    if let Some(provider) = self.piece_provider.as_ref() {
                        self.admit_incoming_peer(
                            incoming,
                            provider.num_pieces(),
                            provider.piece_length(),
                        )
                        .await;
                    }
                }
                SeedWaitEvent::PeerEvent(event) => {
                    self.apply_peer_event(event);
                }
                SeedWaitEvent::Wake => {}
            }
        }

        // The swarm closes event delivery, prevents new actors, and joins all
        // peer tasks; the shared atomic remains authoritative for accounting.
        self.swarm.shutdown_all().await;
        self.total_uploaded = self
            .upload_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        self.publish_upload_stats();

        if let Some(announcer) = self.announcer.as_mut() {
            announcer
                .announce_stopped(
                    &self.info_hash,
                    &self.peer_id,
                    self.total_downloaded,
                    0,
                    self.total_uploaded,
                )
                .await;
        }
        self.is_active = false;
        self.clear_connection_state();
        info!(
            "Seeding loop ended: uploaded {} bytes in {:?}",
            self.total_uploaded,
            self.seeding_duration()
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal loop helpers
    // -----------------------------------------------------------------------

    fn next_seed_event_deadline(&self) -> Instant {
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(24 * 60 * 60);

        if let Some(seed_time) = self.exit_condition.seed_time {
            deadline = deadline.min(self.seeding_start_time + seed_time);
        }
        deadline =
            deadline.min(self.last_choke_time + Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS));
        if let Some(delay) = self
            .announcer
            .as_ref()
            .and_then(|announcer| announcer.next_default_announce_delay())
        {
            deadline = deadline.min(now + delay);
        }
        deadline
    }

    async fn wait_for_seed_event(&mut self, deadline: Instant) -> SeedWaitEvent {
        let cancel_token = self.cancel_token.clone();
        let cancel_wait = cancel_token.cancelled();
        let deadline_wait = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline_wait);
        let incoming_receiver = &mut self.incoming_peers;
        let mut event_receiver = self.swarm.lease_event_receiver();

        let event = tokio::select! {
            incoming = async {
                match incoming_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            } => match incoming {
                Some(incoming) => SeedWaitEvent::Incoming(incoming),
                None => SeedWaitEvent::Wake,
            },
            peer_event = async {
                match event_receiver.as_mut() {
                    Some(receiver) => receiver.recv_unapplied().await,
                    None => std::future::pending().await,
                }
            } => {
                peer_event.map_or(SeedWaitEvent::Wake, SeedWaitEvent::PeerEvent)
            },
            _ = cancel_wait => SeedWaitEvent::Wake,
            _ = &mut deadline_wait => SeedWaitEvent::Wake,
        };

        event
    }

    fn start_seed_peer_actors(&mut self) {
        let Some(provider) = self.piece_provider.as_ref().cloned() else {
            return;
        };
        let sessions = std::mem::take(&mut self.upload_sessions);
        for connection in sessions {
            if let Err(connection) = self
                .swarm
                .spawn_peer(connection, None, Arc::clone(&provider))
            {
                let endpoint = connection.remote_endpoint();
                drop(connection);
                if let Some(endpoint) = endpoint {
                    self.release_peer(endpoint);
                }
            }
        }
    }

    pub(super) fn any_peer_choke_state_mismatch(&self) -> bool {
        self.upload_sessions
            .iter()
            .map(|connection| &connection.stats)
            .chain(self.swarm.iter().map(|actor| &actor.stats))
            .any(|stats| stats.peer_interested == stats.am_choking)
    }

    pub(super) fn apply_peer_event(&mut self, event: PeerEvent) {
        self.swarm.apply_event(&event);
        match event {
            PeerEvent::UploadBytes { .. } => {
                self.total_uploaded = self
                    .upload_counter
                    .load(std::sync::atomic::Ordering::Relaxed);
                self.publish_upload_stats();
            }
            PeerEvent::InterestChanged { .. }
            | PeerEvent::ChokeStateChanged { .. }
            | PeerEvent::PeerChokingChanged { .. }
            | PeerEvent::UploadQueueChanged { .. }
            | PeerEvent::AvailabilityChanged { .. }
            | PeerEvent::PeerAvailabilityChanged { .. }
            | PeerEvent::PeerAvailabilitySnapshot { .. }
            | PeerEvent::PexPeers { .. }
            | PeerEvent::Disconnected { .. }
            | PeerEvent::RequestFailed { .. } => {}
            PeerEvent::Message { actor_id, .. } => {
                tracing::trace!(
                    actor_id = actor_id.0,
                    "Ignoring block message while seeding"
                );
            }
        }
    }

    /// Admit handshaken peers that arrive while the torrent is seeding.
    pub(super) async fn drain_incoming_peers(&mut self) {
        let Some(provider) = self.piece_provider.as_ref() else {
            return;
        };
        let num_pieces = provider.num_pieces();
        let piece_length = provider.piece_length();

        let Some(mut receiver) = self.incoming_peers.take() else {
            return;
        };
        while let Ok(incoming) = receiver.try_recv() {
            self.admit_incoming_peer(incoming, num_pieces, piece_length)
                .await;
        }
        self.incoming_peers = Some(receiver);
    }

    async fn admit_incoming_peer(
        &mut self,
        incoming: crate::engine::bt_peer_listener::IncomingPeer,
        num_pieces: u32,
        piece_length: u32,
    ) {
        let endpoint = incoming.endpoint;
        let mut connection = match incoming.connection {
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(connection) => {
                BtPeerConn::from_incoming_plain(*connection, endpoint)
            }
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                connection,
            ) => BtPeerConn::from_incoming_encrypted(*connection, endpoint),
        };
        connection.configure_upload_with_auto_unchoke(
            &self.config,
            num_pieces,
            piece_length,
            false,
        );
        connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
        connection.stats.am_choking = true;
        let remote_peer_id = connection.remote_peer_id();
        let duplicate = remote_peer_id.is_some_and(|peer_id| {
            peer_id == self.peer_id
                || self
                    .upload_sessions
                    .iter()
                    .any(|active| active.remote_peer_id() == Some(peer_id))
                || self.swarm.has_peer_id(peer_id)
        }) || self.upload_sessions.iter().any(|active| {
            active.remote_ip() == endpoint.ip().to_string()
                && active.remote_port() == endpoint.port()
        }) || self.swarm.has_endpoint(endpoint);

        if duplicate {
            debug!(%endpoint, remote_peer_id = ?remote_peer_id, "Rejected duplicate or self BitTorrent seed peer");
            self.release_peer(endpoint);
            return;
        }

        if let Some(provider) = self.piece_provider.as_ref().cloned() {
            let Some(mut coordinator) = self.swarm.lease_event_receiver() else {
                self.release_peer(endpoint);
                self.publish_connection_state();
                return;
            };
            let actor_id = match coordinator.spawn_peer(connection, None, provider) {
                Ok(actor_id) => actor_id,
                Err(connection) => {
                    drop(coordinator);
                    drop(connection);
                    self.release_peer(endpoint);
                    self.publish_connection_state();
                    return;
                }
            };
            if let Some(actor) = coordinator.actor_mut(actor_id)
                && let Err(error) = actor.try_send(PeerCommand::AnnounceAvailability)
            {
                actor.dead = true;
                debug!(%endpoint, %error, "Failed to queue seed peer availability announcement");
            }
            drop(coordinator);
        } else {
            self.upload_sessions.push(connection);
        }
        self.publish_connection_state();
        info!(%endpoint, "Admitted incoming BitTorrent seed peer");
    }

    fn release_peer(&self, endpoint: std::net::SocketAddr) {
        if let Some(peer_storage) = &self.peer_storage {
            peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
        }
    }

    /// Remove disconnected peers and report whether upstream requires an
    /// immediate choke round for a returned unchoked, interested peer.
    pub(super) async fn remove_dead_sessions(&mut self) -> bool {
        if !self.swarm.is_empty() {
            let before = self.swarm.len();
            let needs_choke_round = self
                .swarm
                .iter()
                .any(|actor| actor.dead && actor.stats.peer_interested && !actor.stats.am_choking);
            let removed = self.swarm.remove_dead().await;
            for (_, endpoint) in &removed {
                self.release_peer(*endpoint);
            }
            let removed_count = before - self.swarm.len();
            if removed_count > 0 {
                debug!("Removed {} dead seeding peer actors", removed_count);
            }
            return needs_choke_round;
        }
        let before = self.upload_sessions.len();
        // Collect indices of dead sessions
        let dead_indices: Vec<usize> = self
            .upload_sessions
            .iter()
            .enumerate()
            .filter(|(_, connection)| !connection.is_connected())
            .map(|(i, _)| i)
            .collect();
        let needs_choke_round = dead_indices.iter().any(|&index| {
            let stats = &self.upload_sessions[index].stats;
            stats.peer_interested && !stats.am_choking
        });

        // Remove in reverse order to keep indices stable
        for idx in dead_indices.into_iter().rev() {
            if let Some(endpoint) = self.upload_sessions[idx].remote_endpoint()
                && let Some(peer_storage) = &self.peer_storage
            {
                peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
            }
            self.upload_sessions.remove(idx);
        }

        let removed = before - self.upload_sessions.len();
        if removed > 0 {
            debug!("Removed {} dead upload sessions", removed);
        }
        needs_choke_round
    }

    /// Refresh speed estimates from the authoritative connection and actor stats.
    fn sync_sessions_to_stats(&mut self) {
        let elapsed = self.seeding_start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            for connection in &mut self.upload_sessions {
                connection.stats.upload_speed = connection.stats.uploaded_bytes as f64 / elapsed;
            }
            for actor in self.swarm.iter_mut() {
                actor.stats.upload_speed = actor.stats.uploaded_bytes as f64 / elapsed;
            }
        }
    }

    /// Compute choke decisions from actor-owned stats and apply the results.
    async fn run_choke_round(&mut self) {
        let mut desired_stats = self
            .upload_sessions
            .iter()
            .map(|connection| connection.stats.clone())
            .chain(self.swarm.iter().map(|actor| actor.stats.clone()))
            .collect::<Vec<_>>();
        let mut peers_mut: Vec<&mut PeerStats> = desired_stats.iter_mut().collect();
        self.seeder_choke.execute_choke(&mut peers_mut[..]);

        let desired_choking = desired_stats
            .into_iter()
            .map(|stats| stats.am_choking)
            .collect::<Vec<_>>();
        let upload_count = self.upload_sessions.len();
        for (connection, desired) in self
            .upload_sessions
            .iter_mut()
            .zip(desired_choking.iter().copied())
        {
            if desired && !connection.stats.am_choking {
                if let Err(error) = connection.choke_upload_peer().await {
                    warn!("Failed to choke peer: {}", error);
                }
            } else if !desired
                && connection.stats.am_choking
                && let Err(error) = connection.unchoke_upload_peer().await
            {
                warn!("Failed to unchoke peer: {}", error);
            }
        }
        for (actor, desired) in self
            .swarm
            .iter()
            .zip(desired_choking.into_iter().skip(upload_count))
        {
            if desired == actor.stats.am_choking {
                continue;
            }
            let command = if desired {
                PeerCommand::ChokeUpload
            } else {
                PeerCommand::UnchokeUpload
            };
            if let Err(error) = self.swarm.send_to(actor.actor_id, command).await {
                warn!(
                    actor_id = actor.actor_id.0,
                    "Failed to send choke decision to peer actor: {}", error
                );
            }
        }
    }
}
