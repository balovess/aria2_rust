use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::engine::bt_upload_session::{BtUploadConnection, BtUploadSession};
use crate::engine::peer_stats::PeerStats;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS};

enum SeedWaitEvent {
    Incoming(crate::engine::bt_peer_listener::IncomingPeer),
    PeerMessage {
        index: usize,
        result: crate::error::Result<u64>,
    },
    Wake,
}

impl BtSeedManager {
    // -----------------------------------------------------------------------
    // Main seeding loop
    // -----------------------------------------------------------------------

    /// Run the main seeding loop until exit conditions are met or cancelled.
    ///
    /// Mirrors C++ `SeedCheckCommand::execute()` combined with upload session
    /// management while waiting on actual peer, listener, cancellation, and
    /// deadline events instead of scanning on a fixed interval.
    pub async fn run_seeding_loop(&mut self) -> crate::error::Result<()> {
        info!(
            info_hash = ?self.info_hash,
            "Seeding loop started (ratio={:?}, time={:?}, peers={})",
            self.exit_condition.seed_ratio,
            self.exit_condition.seed_time,
            self.upload_sessions.len()
        );

        if let Some(provider) = self.piece_provider.as_ref() {
            for session in &mut self.upload_sessions {
                if let Err(error) = session.send_piece_availability(provider.as_ref()).await {
                    warn!(%error, "Failed to announce completed BitTorrent seed availability");
                    session.is_dead = true;
                }
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
                    "[Seed] Re-announced to {} ({} seeders, {} leechers)",
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
            let interested_peer_needs_decision = self
                .upload_sessions
                .iter()
                .any(|session| session.is_peer_interested() && session.is_peer_choked());
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
                SeedWaitEvent::PeerMessage { index, result } => {
                    if let Ok(bytes) = result {
                        self.total_uploaded = self.total_uploaded.saturating_add(bytes);
                        self.publish_upload_stats();
                    } else if let Some(session) = self.upload_sessions.get_mut(index) {
                        session.is_dead = true;
                    }
                }
                SeedWaitEvent::Wake => {}
            }
        }

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
        let incoming_wait = async {
            match self.incoming_peers.as_mut() {
                Some(receiver) => receiver.recv().await,
                None => {
                    std::future::pending::<Option<crate::engine::bt_peer_listener::IncomingPeer>>()
                        .await
                }
            }
        };
        tokio::pin!(incoming_wait);

        let provider = self.piece_provider.clone();
        let mut peer_reads = futures::stream::FuturesUnordered::new();
        if let Some(provider) = provider {
            for (index, session) in self
                .upload_sessions
                .iter_mut()
                .enumerate()
                .filter(|(_, session)| !session.is_dead())
            {
                let provider = Arc::clone(&provider);
                peer_reads.push(async move {
                    (
                        index,
                        session.handle_incoming_messages(provider.as_ref()).await,
                    )
                });
            }
        }

        let event = tokio::select! {
            incoming = &mut incoming_wait => match incoming {
                Some(incoming) => SeedWaitEvent::Incoming(incoming),
                None => SeedWaitEvent::Wake,
            },
            peer = peer_reads.next(), if !peer_reads.is_empty() => {
                peer.map_or(SeedWaitEvent::Wake, |(index, result)| {
                    SeedWaitEvent::PeerMessage { index, result }
                })
            },
            _ = cancel_wait => SeedWaitEvent::Wake,
            _ = &mut deadline_wait => SeedWaitEvent::Wake,
        };

        drop(peer_reads);
        event
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
        let remote_peer_id = incoming.connection.remote_peer_id();
        let duplicate = remote_peer_id.is_some_and(|peer_id| {
            peer_id == self.peer_id
                || self
                    .upload_sessions
                    .iter()
                    .any(|session| session.remote_peer_id() == Some(peer_id))
        }) || self.upload_sessions.iter().any(|session| {
            session.endpoint() == Some((endpoint.ip().to_string(), endpoint.port()))
        });

        if duplicate {
            debug!(%endpoint, remote_peer_id = ?remote_peer_id, "Rejected duplicate or self BitTorrent seed peer");
            self.release_peer(endpoint);
            return;
        }

        let transport = match incoming.connection {
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(connection) => {
                BtUploadConnection::Plain(connection)
            }
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                connection,
            ) => BtUploadConnection::Encrypted(connection),
        };
        let mut session = BtUploadSession::new_with_connection(transport, &self.config);
        session.configure_message_validator(num_pieces, piece_length);
        if let Some(provider) = self.piece_provider.as_ref()
            && let Err(error) = session.send_piece_availability(provider.as_ref()).await
        {
            debug!(%endpoint, %error, "Failed to announce completed BitTorrent seed availability");
            self.release_peer(endpoint);
            warn!(%endpoint, %error, "Failed to announce BitTorrent seed availability");
            return;
        }
        let peer_stats = PeerStats::new(remote_peer_id.unwrap_or([0u8; 20]), endpoint);
        self.upload_sessions.push(session);
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

    /// Remove upload sessions whose connections have died.
    fn remove_dead_sessions(&mut self) {
        let before = self.upload_sessions.len();
        // Collect indices of dead sessions
        let dead_indices: Vec<usize> = self
            .upload_sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_dead())
            .map(|(i, _)| i)
            .collect();

        // Remove in reverse order to keep indices stable
        for idx in dead_indices.into_iter().rev() {
            if let Some((ip, port)) = self.upload_sessions[idx].endpoint()
                && let Some(peer_storage) = &self.peer_storage
            {
                peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .return_peer_by_endpoint(&ip, port);
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

    /// Sync state from upload sessions to PeerStats.
    ///
    /// The upload sessions own the authoritative `peer_interested` and
    /// `uploaded_bytes` values (updated by incoming message handling).
    /// Before running the choking algorithm, we propagate these values
    /// to the PeerStats so the algorithm sees the latest state.
    fn sync_sessions_to_stats(&mut self) {
        let len = self.upload_sessions.len().min(self.peer_stats.len());
        for i in 0..len {
            let session = &self.upload_sessions[i];
            let stats = &mut self.peer_stats[i];
            stats.peer_interested = session.is_peer_interested();
            stats.uploaded_bytes = session.uploaded_bytes();
            // Estimate upload speed from session's bytes and elapsed time
            let elapsed = self.seeding_start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                stats.upload_speed = session.uploaded_bytes() as f64 / elapsed;
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

    /// Apply choke/unchoke decisions from PeerStats to upload sessions.
    ///
    /// After the choking algorithm sets `am_choking` on PeerStats, we send
    /// the corresponding Choke/Unchoke messages to peers via their upload
    /// sessions.
    async fn apply_choke_decisions(&mut self) {
        let len = self.upload_sessions.len().min(self.peer_stats.len());
        for i in 0..len {
            let stats_am_choking = self.peer_stats[i].am_choking;
            let session = &mut self.upload_sessions[i];

            if stats_am_choking && !session.is_peer_choked() {
                // Need to choke this peer
                if let Err(e) = session.choke_peer().await {
                    warn!("Failed to choke peer: {}", e);
                }
            } else if !stats_am_choking && session.is_peer_choked() {
                // Need to unchoke this peer
                if let Err(e) = session.unchoke_peer().await {
                    warn!("Failed to unchoke peer: {}", e);
                }
            }
        }
    }
}
