use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::PeerConnectionResult;
use crate::engine::bittorrent::peer::stats::PeerStats;
use crate::util::rwlock_ext::RwLockRecover;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS};
use crate::engine::bittorrent::peer::message_handler::{PeerCommand, PeerEvent};

enum SeedWaitEvent {
    Incoming(Box<crate::engine::bittorrent::peer::listener::IncomingPeer>),
    PeerEvent(PeerEvent),
    TrackerAnnounce(
        Result<
            Option<crate::engine::bittorrent::tracker::communication::AnnounceResult>,
            tokio::task::JoinError,
        >,
    ),
    PeerConnections(Result<crate::error::Result<PeerConnectionResult>, tokio::task::JoinError>),
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
        let upload_speed_reporter = self.upload_progress.as_ref().map(|progress| {
            crate::engine::bittorrent::download::execute::spawn_upload_speed_reporter(
                Arc::clone(progress),
                self.swarm.upload_rate(),
            )
        });
        info!(
            info_hash = ?self.info_hash,
            "Seeding loop started (ratio={:?}, time={:?}, active_peers={}, pending_connections={})",
            self.exit_condition.seed_ratio,
            self.exit_condition.seed_time,
            self.swarm.len(),
            self.pending_connections.len()
        );

        self.start_seed_peer_actors();
        self.remove_dead_sessions().await;
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

            // The listener remains active after the payload is complete. This
            // is the Rust equivalent of PeerListenCommand continuing beside
            // SeedCheckCommand, including when no peer was connected at the
            // instant the download finished.
            self.drain_incoming_peers().await;
            self.collect_periodic_dht_peers().await;
            self.start_tracker_announce();

            // -- Process state changes observed since the last event ---------
            let removed_peer_needs_choke_round = self.remove_dead_sessions().await;
            self.publish_connection_state();
            self.publish_upload_stats();
            let peer_choke_state_needs_decision = self.any_peer_choke_state_mismatch();
            if removed_peer_needs_choke_round
                || self.last_choke_time.elapsed().as_secs() >= CHOKE_ROUND_INTERVAL_SECS
                || peer_choke_state_needs_decision
            {
                self.run_choke_round();
                self.last_choke_time = Instant::now();
            }
            let peer_exchange_enabled = self
                .peer_discovery
                .as_ref()
                .is_some_and(|discovery| discovery.enable_peer_exchange);
            crate::engine::bittorrent::download::execute::send_periodic_pex_to_swarm(
                &mut self.swarm,
                &mut self.last_pex_send,
                peer_exchange_enabled,
            )
            .await;
            self.start_peer_connection_attempt();

            // Park until a socket message, an incoming peer, cancellation, or
            // a protocol/seed deadline wakes the manager. There is no fixed
            // interval scan when the swarm is idle.
            match self
                .wait_for_seed_event(self.next_seed_event_deadline())
                .await
            {
                SeedWaitEvent::Incoming(incoming) => {
                    self.admit_incoming_peer(*incoming).await;
                }
                SeedWaitEvent::PeerEvent(event) => {
                    self.apply_peer_event(event);
                }
                SeedWaitEvent::TrackerAnnounce(result) => {
                    self.finish_tracker_announce(result);
                }
                SeedWaitEvent::PeerConnections(result) => {
                    self.finish_peer_connection_attempt(result);
                }
                SeedWaitEvent::Wake => {}
            }
        }

        self.cancel_peer_connection_attempt().await;
        self.finish_pending_tracker_announce().await;
        if let Some(discovery) = self.peer_discovery.as_mut() {
            discovery.dht_lookup.cancel_pending_lookup().await;
        }
        // The swarm closes event delivery, prevents new actors, and joins all
        // peer tasks; the shared atomic remains authoritative for accounting.
        self.swarm.shutdown_all().await;
        if let Some(reporter) = upload_speed_reporter {
            reporter.abort();
            let _ = reporter.await;
        }
        self.total_uploaded = self
            .upload_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        self.publish_upload_stats();

        if let Some(actor) = self.tracker_actor.take() {
            self.announcer = actor
                .stop()
                .await
                .map(|announcer| Arc::new(tokio::sync::Mutex::new(announcer)));
        } else if let Some(announcer) = self.announcer.as_ref() {
            announcer
                .lock()
                .await
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
        if self
            .peer_discovery
            .as_ref()
            .is_some_and(|discovery| discovery.enable_peer_exchange)
        {
            deadline = deadline.min(
                self.last_pex_send
                    + crate::engine::bittorrent::download::execute::PEX_SEND_INTERVAL,
            );
        }
        if self.pending_tracker_announce.is_none()
            && let Some(delay) = self
                .announcer
                .as_ref()
                .and_then(|announcer| announcer.try_lock().ok()?.next_default_announce_delay())
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(upload_speed_deadline) = self
            .swarm
            .iter()
            .filter_map(|actor| actor.stats.next_upload_speed_deadline(now))
            .min()
        {
            deadline = deadline.min(upload_speed_deadline);
        }
        if let Some(discovery) = self.peer_discovery.as_ref()
            && !discovery.dht_engines.is_empty()
            && let Some(delay) = discovery.dht_lookup.next_lookup_delay(self.swarm.len())
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
        let incoming_receiver = self.incoming_peers.clone();
        let mut event_receiver = self.swarm.lease_event_receiver();
        let dht_notifier = self.peer_discovery.as_ref().and_then(|discovery| {
            (!discovery.dht_engines.is_empty()).then(|| discovery.dht_lookup.completion_notifier())
        });
        let pending_connections = self
            .pending_peer_connection
            .as_mut()
            .map(|attempt| &mut attempt.task);
        let pending_tracker = self.pending_tracker_announce.as_mut();

        let event = tokio::select! {
            incoming = async move {
                match incoming_receiver {
                    Some(receiver) => receiver.lock().await.recv().await,
                    None => std::future::pending().await,
                }
            } => match incoming {
                Some(incoming) => SeedWaitEvent::Incoming(Box::new(incoming)),
                None => {
                    self.incoming_peers = None;
                    SeedWaitEvent::Wake
                }
            },
            peer_event = async {
                match event_receiver.as_mut() {
                    Some(receiver) => receiver.recv_unapplied().await,
                    None => std::future::pending().await,
                }
            } => {
                peer_event.map_or(SeedWaitEvent::Wake, SeedWaitEvent::PeerEvent)
            },
            result = async {
                match pending_tracker {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => SeedWaitEvent::TrackerAnnounce(result),
            result = async {
                match pending_connections {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => SeedWaitEvent::PeerConnections(result),
            _ = async {
                match dht_notifier {
                    Some(notifier) => notifier.notified().await,
                    None => std::future::pending().await,
                }
            } => SeedWaitEvent::Wake,
            _ = cancel_wait => SeedWaitEvent::Wake,
            _ = &mut deadline_wait => SeedWaitEvent::Wake,
        };

        event
    }

    fn start_tracker_announce(&mut self) {
        if self.tracker_actor.is_some() || self.pending_tracker_announce.is_some() {
            return;
        }
        let Some(announcer) = self.announcer.as_ref() else {
            return;
        };
        let ready = announcer
            .try_lock()
            .is_ok_and(|announcer| announcer.is_default_announce_ready());
        if !ready {
            return;
        }

        let announcer = Arc::clone(announcer);
        let info_hash = self.info_hash;
        let peer_id = self.peer_id;
        let downloaded = self.total_downloaded;
        let uploaded = self.total_uploaded;
        self.pending_tracker_announce = Some(tokio::spawn(async move {
            announcer
                .lock()
                .await
                .announce(&info_hash, &peer_id, downloaded, 0, uploaded)
                .await
        }));
    }

    fn finish_tracker_announce(
        &mut self,
        result: Result<
            Option<crate::engine::bittorrent::tracker::communication::AnnounceResult>,
            tokio::task::JoinError,
        >,
    ) {
        self.pending_tracker_announce.take();
        match result {
            Ok(Some(result)) => {
                debug!(
                    "[Seed] Re-announced to {} ({:?} seeders, {:?} leechers)",
                    result.tracker_url, result.seeders, result.leechers
                );
                self.store_tracker_peers(result.peers);
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "Seeding tracker announce task failed"),
        }
    }

    async fn finish_pending_tracker_announce(&mut self) {
        let Some(task) = self.pending_tracker_announce.take() else {
            return;
        };
        if let Err(error) = task.await {
            warn!(%error, "Seeding tracker announce task failed during shutdown");
        }
    }

    fn start_seed_peer_actors(&mut self) {
        let connections = std::mem::take(&mut self.pending_connections);
        let dht_engines = self
            .peer_discovery
            .as_ref()
            .map(|discovery| discovery.dht_engines.clone());
        for mut connection in connections {
            self.prepare_actor_startup(&mut connection);
            let dht_engine = connection
                .remote_endpoint()
                .and_then(|endpoint| dht_engines.as_ref()?.for_peer(endpoint));
            if let Err(connection) =
                self.swarm
                    .spawn_peer(connection, dht_engine, Arc::clone(&self.piece_provider))
            {
                let endpoint = connection.remote_endpoint();
                drop(connection);
                if let Some(endpoint) = endpoint {
                    self.release_peer(endpoint);
                }
            }
        }
    }

    fn prepare_actor_startup(&self, connection: &mut BtPeerConn) {
        let (peer_agent, listen_port) = self
            .peer_discovery
            .as_ref()
            .map(|discovery| {
                (
                    discovery.connection_options.peer_agent.clone(),
                    discovery.connection_options.listen_port,
                )
            })
            .unwrap_or_else(|| {
                (
                    aria2_protocol::identity::DEFAULT_PEER_AGENT.to_string(),
                    None,
                )
            });
        connection.prepare_actor_startup(
            peer_agent,
            listen_port,
            &self.info_hash,
            self.piece_provider.num_pieces(),
        );
    }

    pub(super) fn any_peer_choke_state_mismatch(&self) -> bool {
        self.swarm
            .iter()
            .map(|actor| &actor.stats)
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
            | PeerEvent::AmInterestChanged { .. }
            | PeerEvent::ChokeStateChanged { .. }
            | PeerEvent::PeerChokingChanged { .. }
            | PeerEvent::UploadQueueChanged { .. }
            | PeerEvent::PeerAvailabilityChanged { .. }
            | PeerEvent::PeerAvailabilitySnapshot { .. }
            | PeerEvent::AllowedFast { .. }
            | PeerEvent::ExtensionHandshakeReceived { .. }
            | PeerEvent::Disconnected { .. }
            | PeerEvent::GracefulDisconnected { .. }
            | PeerEvent::RequestFailed { .. } => {}
            PeerEvent::PexPeers { peers } => {
                if self
                    .peer_discovery
                    .as_ref()
                    .is_some_and(|discovery| discovery.enable_peer_exchange)
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
    pub(super) async fn drain_incoming_peers(&mut self) {
        loop {
            let Some(receiver) = self.incoming_peers.as_ref().cloned() else {
                return;
            };
            let incoming = receiver.lock().await.try_recv();
            match incoming {
                Ok(incoming) => self.admit_incoming_peer(incoming).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.incoming_peers = None;
                    return;
                }
            }
        }
    }

    async fn admit_incoming_peer(
        &mut self,
        incoming: crate::engine::bittorrent::peer::listener::IncomingPeer,
    ) {
        let endpoint = incoming.endpoint;
        if let Some(discovery) = self.peer_discovery.as_ref() {
            let max_peers = discovery.group.recover().options().bt_max_peers;
            let pending = self
                .pending_peer_connection
                .as_ref()
                .map_or(0, |attempt| attempt.checked_out.len());
            if max_peers > 0
                && self.swarm.len() + self.pending_connections.len() + pending >= max_peers
            {
                debug!(%endpoint, max_peers, "Rejected incoming seeding peer at the configured peer limit");
                drop(incoming);
                self.release_peer(endpoint);
                return;
            }
        }
        let mut connection = BtPeerConn::from_incoming_tcp(incoming.connection, endpoint);
        connection.set_pex_enabled(
            self.peer_discovery
                .as_ref()
                .is_some_and(|discovery| discovery.enable_peer_exchange),
        );
        connection.configure_upload_with_auto_unchoke(
            &self.config,
            self.torrent_upload_limiter.clone(),
            self.piece_provider.num_pieces(),
            self.piece_provider.piece_length(),
            false,
        );
        self.prepare_actor_startup(&mut connection);
        connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
        connection.stats.am_choking = true;
        let remote_peer_id = connection.remote_peer_id();
        let duplicate = remote_peer_id == Some(self.peer_id)
            || remote_peer_id.is_some_and(|peer_id| self.swarm.has_peer_id(peer_id))
            || self.swarm.has_endpoint(endpoint);

        if duplicate {
            debug!(%endpoint, remote_peer_id = ?remote_peer_id, "Rejected duplicate or self BitTorrent seed peer");
            self.release_peer(endpoint);
            return;
        }

        let dht_engines = self
            .peer_discovery
            .as_ref()
            .map(|discovery| discovery.dht_engines.clone());

        let Some(mut coordinator) = self.swarm.lease_event_receiver() else {
            self.release_peer(endpoint);
            self.publish_connection_state();
            return;
        };
        let actor_id = match coordinator.spawn_peer(
            connection,
            dht_engines
                .as_ref()
                .and_then(|engines| engines.for_peer(endpoint)),
            Arc::clone(&self.piece_provider),
        ) {
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
        self.publish_connection_state();
        info!(%endpoint, "Admitted incoming BitTorrent seed peer");
    }

    fn release_peer(&mut self, endpoint: std::net::SocketAddr) {
        self.peer_sources.remove(&endpoint);
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
    fn run_choke_round(&mut self) {
        let mut desired_stats = self
            .swarm
            .iter()
            .map(|actor| actor.stats.clone())
            .collect::<Vec<_>>();
        let mut peers_mut: Vec<&mut PeerStats> = desired_stats.iter_mut().collect();
        self.seeder_choke.execute_choke(&mut peers_mut[..]);

        let desired_choking = desired_stats
            .into_iter()
            .map(|stats| stats.am_choking)
            .collect::<Vec<_>>();
        for (actor, desired) in self.swarm.iter().zip(desired_choking) {
            if desired == actor.stats.am_choking {
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
