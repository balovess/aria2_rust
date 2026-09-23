use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::peer_stats::PeerStats;

use super::peer_actor::{PeerCommand, PeerEvent, SeedPeerActor};
use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS};

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

        if let Some(provider) = self.piece_provider.as_ref() {
            let mut failed_indices = Vec::new();
            for (index, connection) in self.upload_sessions.iter_mut().enumerate() {
                if let Err(error) = send_piece_availability(connection, provider.as_ref()).await {
                    warn!(%error, "Failed to announce completed BitTorrent seed availability");
                    failed_indices.push(index);
                }
            }
            for index in failed_indices.into_iter().rev() {
                self.upload_sessions.remove(index);
                if index < self.peer_stats.len() {
                    self.peer_stats.remove(index);
                }
            }
        }
        self.remove_dead_sessions();
        self.start_seed_peer_actors();
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
            self.remove_dead_sessions();
            self.sync_sessions_to_stats();
            self.publish_connection_state();
            let interested_peer_needs_decision = self.any_interested_peer_is_choked();
            if self.last_choke_time.elapsed().as_secs() >= CHOKE_ROUND_INTERVAL_SECS
                || interested_peer_needs_decision
            {
                self.run_choke_round();
                self.last_choke_time = Instant::now();
            }
            self.apply_choke_decisions().await;

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

        // Release the event receiver so actors blocked on a full queue can exit;
        // the shared atomic remains authoritative for final accounting.
        self.seed_peer_event_rx = None;
        for actor in &mut self.seed_peer_actors {
            actor.shutdown().await;
        }
        self.seed_peer_actors.clear();
        self.seed_peer_actor_indices.clear();
        self.seed_peer_event_tx = None;
        self.seed_peer_event_rx = None;
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
        let event_receiver = &mut self.seed_peer_event_rx;

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
                    Some(receiver) => receiver.recv().await,
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
        if self.seed_peer_event_tx.is_some() {
            return;
        }
        let Some(provider) = self.piece_provider.as_ref().cloned() else {
            return;
        };
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
        self.seed_peer_event_tx = Some(event_tx.clone());
        self.seed_peer_event_rx = Some(event_rx);
        let sessions = std::mem::take(&mut self.upload_sessions);
        for connection in sessions {
            let actor_id = self.next_seed_peer_actor_id as usize;
            self.next_seed_peer_actor_id = self.next_seed_peer_actor_id.wrapping_add(1);
            let actor_index = self.seed_peer_actors.len();
            self.seed_peer_actors.push(SeedPeerActor::spawn(
                actor_id,
                connection,
                Arc::clone(&provider),
                event_tx.clone(),
            ));
            self.seed_peer_actor_indices.insert(actor_id, actor_index);
        }
    }

    fn any_interested_peer_is_choked(&self) -> bool {
        self.peer_stats
            .iter()
            .any(|stats| stats.peer_interested && stats.am_choking)
    }

    fn apply_peer_event(&mut self, event: PeerEvent) {
        match event {
            PeerEvent::UploadBytes {
                peer_index,
                snapshot,
            } => {
                self.update_actor_stats(peer_index, *snapshot);
                self.total_uploaded = self
                    .upload_counter
                    .load(std::sync::atomic::Ordering::Relaxed);
                self.publish_upload_stats();
            }
            PeerEvent::InterestChanged {
                peer_index,
                snapshot,
            } => {
                self.update_actor_stats(peer_index, *snapshot);
            }
            PeerEvent::Disconnected { peer_index } => {
                if let Some(&actor_index) = self.seed_peer_actor_indices.get(&peer_index)
                    && let Some(actor) = self.seed_peer_actors.get_mut(actor_index)
                {
                    actor.dead = true;
                }
            }
            PeerEvent::RequestFailed { peer_index, .. } => {
                if let Some(&actor_index) = self.seed_peer_actor_indices.get(&peer_index)
                    && let Some(actor) = self.seed_peer_actors.get_mut(actor_index)
                {
                    actor.dead = true;
                }
            }
            PeerEvent::Message { .. } => {}
        }
    }

    fn update_actor_stats(&mut self, actor_id: usize, snapshot: PeerStats) {
        if let Some(&actor_index) = self.seed_peer_actor_indices.get(&actor_id)
            && let Some(stats) = self.peer_stats.get_mut(actor_index)
        {
            *stats = snapshot;
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
                || self
                    .seed_peer_actors
                    .iter()
                    .any(|actor| actor.endpoint == endpoint)
        }) || self.upload_sessions.iter().any(|active| {
            active.remote_ip() == endpoint.ip().to_string()
                && active.remote_port() == endpoint.port()
        }) || self
            .seed_peer_actors
            .iter()
            .any(|actor| actor.endpoint == endpoint);

        if duplicate {
            debug!(%endpoint, remote_peer_id = ?remote_peer_id, "Rejected duplicate or self BitTorrent seed peer");
            self.release_peer(endpoint);
            return;
        }

        if let Some(provider) = self.piece_provider.as_ref()
            && let Err(error) = send_piece_availability(&mut connection, provider.as_ref()).await
        {
            debug!(%endpoint, %error, "Failed to announce completed BitTorrent seed availability");
            self.release_peer(endpoint);
            warn!(%endpoint, %error, "Failed to announce BitTorrent seed availability");
            return;
        }
        let peer_stats = connection.stats.clone();
        if let (Some(provider), Some(event_tx)) = (
            self.piece_provider.as_ref(),
            self.seed_peer_event_tx.as_ref(),
        ) {
            let actor_id = self.next_seed_peer_actor_id as usize;
            self.next_seed_peer_actor_id = self.next_seed_peer_actor_id.wrapping_add(1);
            let actor_index = self.seed_peer_actors.len();
            self.seed_peer_actors.push(SeedPeerActor::spawn(
                actor_id,
                connection,
                std::sync::Arc::clone(provider),
                event_tx.clone(),
            ));
            self.seed_peer_actor_indices.insert(actor_id, actor_index);
        } else {
            self.upload_sessions.push(connection);
        }
        self.peer_stats.push(peer_stats);
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

    /// Remove disconnected peer workers or not-yet-started connections.
    fn remove_dead_sessions(&mut self) {
        if !self.seed_peer_actors.is_empty() || self.seed_peer_event_tx.is_some() {
            let before = self.seed_peer_actors.len();
            let mut index = self.seed_peer_actors.len();
            while index > 0 {
                index -= 1;
                let actor = &self.seed_peer_actors[index];
                if actor.dead {
                    self.release_peer(actor.endpoint);
                    self.seed_peer_actor_indices.remove(&actor.actor_id);
                    self.seed_peer_actors.remove(index);
                    if index < self.peer_stats.len() {
                        self.peer_stats.remove(index);
                    }
                }
            }
            self.peer_stats.truncate(self.seed_peer_actors.len());
            for (actor_index, actor) in self.seed_peer_actors.iter().enumerate() {
                self.seed_peer_actor_indices
                    .insert(actor.actor_id, actor_index);
            }
            let removed = before - self.seed_peer_actors.len();
            if removed > 0 {
                debug!("Removed {} dead seeding peer actors", removed);
            }
            return;
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
            if idx < self.peer_stats.len() {
                self.peer_stats.remove(idx);
            }
        }

        // Keep the parallel statistics vector aligned even if it was already
        // out of sync before dead sessions were removed.
        self.peer_stats.truncate(self.upload_sessions.len());

        let removed = before - self.upload_sessions.len();
        if removed > 0 {
            debug!("Removed {} dead upload sessions", removed);
        }
    }

    /// Sync state from pre-worker connections and peer events to PeerStats.
    ///
    /// Peer connections own the authoritative `peer_interested` and
    /// `uploaded_bytes` values. Events update the corresponding peer stats
    /// before the choking algorithm runs.
    fn sync_sessions_to_stats(&mut self) {
        let len = self.num_sessions().min(self.peer_stats.len());
        for i in 0..len {
            let (interested, uploaded_bytes) = if let Some(session) = self.upload_sessions.get(i) {
                (session.stats.peer_interested, session.stats.uploaded_bytes)
            } else {
                let stats = &self.peer_stats[i];
                (stats.peer_interested, stats.uploaded_bytes)
            };
            let stats = &mut self.peer_stats[i];
            stats.peer_interested = interested;
            stats.uploaded_bytes = uploaded_bytes;
            // Estimate upload speed from session's bytes and elapsed time
            let elapsed = self.seeding_start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                stats.upload_speed = uploaded_bytes as f64 / elapsed;
            }
        }
    }

    /// Run one round of the seeder-state choking algorithm.
    fn run_choke_round(&mut self) {
        // Take ownership of peer_stats temporarily so the choking algorithm
        // can modify them through &mut references without aliasing self.
        let mut peer_stats = std::mem::take(&mut self.peer_stats);

        // Build mutable slice references for the choking algorithm
        let mut peers_mut: Vec<&mut PeerStats> = peer_stats.iter_mut().collect();
        self.seeder_choke.execute_choke(&mut peers_mut[..]);

        // Restore peer_stats
        self.peer_stats = peer_stats;
    }

    /// Apply choke/unchoke decisions from PeerStats to peer workers.
    ///
    /// After the choking algorithm sets `am_choking` on PeerStats, send the
    /// corresponding Choke/Unchoke commands through each worker channel.
    async fn apply_choke_decisions(&mut self) {
        let len = self.num_sessions().min(self.peer_stats.len());
        for i in 0..len {
            let stats_am_choking = self.peer_stats[i].am_choking;
            if let Some(connection) = self.upload_sessions.get_mut(i) {
                if stats_am_choking && !connection.stats.am_choking {
                    if let Err(error) = connection.choke_upload_peer().await {
                        warn!("Failed to choke peer: {}", error);
                    }
                } else if !stats_am_choking
                    && connection.stats.am_choking
                    && let Err(error) = connection.unchoke_upload_peer().await
                {
                    warn!("Failed to unchoke peer: {}", error);
                }
                continue;
            }
            if let Some(actor) = self.seed_peer_actors.get(i - self.upload_sessions.len()) {
                let command = if stats_am_choking {
                    PeerCommand::ChokeUpload
                } else {
                    PeerCommand::UnchokeUpload
                };
                if let Err(error) = actor.send(command).await {
                    warn!("Failed to send choke decision to peer actor: {}", error);
                }
            }
        }
    }
}

async fn send_piece_availability(
    connection: &mut BtPeerConn,
    provider: &dyn crate::engine::bt_upload_session::PieceDataProvider,
) -> crate::error::Result<()> {
    let num_pieces = provider.num_pieces();
    if num_pieces == 0 {
        return connection.send_have_none().await;
    }

    let mut bitfield = vec![0u8; (num_pieces as usize).div_ceil(8)];
    for piece_index in 0..num_pieces {
        if provider.has_piece(piece_index) {
            bitfield[piece_index as usize / 8] |= 1 << (7 - piece_index % 8);
        }
    }
    connection.send_bitfield(bitfield).await
}
