use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::engine::bittorrent::peer::interaction::PeerConnectionResult;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS};

mod peer_admission;
mod peer_state;
mod tracker;
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
    PeerConnections(Result<PeerConnectionResult, tokio::task::JoinError>),
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
        self.total_uploaded = self
            .upload_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        self.publish_upload_stats();

        if let Some(actor) = self.tracker_actor.take() {
            actor.stop().await;
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
}
