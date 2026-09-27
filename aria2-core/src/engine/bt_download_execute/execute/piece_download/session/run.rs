use std::time::Instant;
use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};

use crate::engine::bt_download_execute::execute::incoming::PeerActorUploadContext;
use crate::engine::bt_download_execute::types::PeerKey;
use crate::engine::bt_message_handler::{PeerCommand, PeerEvent};
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::engine::choking_algorithm::PeerIdentity;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    ActiveConnectionGuard, BtPeerSource, DownloadResultCode, HaltReason,
};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

use super::{PieceDownloadSession, PieceLoopAction};

const MAX_WEB_SEED_PIECES_IN_FLIGHT: usize = 4;

#[derive(Default)]
struct WebSeedRetries {
    due: BinaryHeap<Reverse<(Instant, u32)>>,
    attempts: std::collections::HashMap<u32, u32>,
}

impl WebSeedRetries {
    fn schedule(&mut self, piece_index: u32, max_retries: u32, retry_wait: Duration) {
        let attempts = self.attempts.entry(piece_index).or_default();
        if *attempts >= max_retries {
            return;
        }
        *attempts += 1;
        let now = Instant::now();
        let deadline = now.checked_add(retry_wait).unwrap_or(now);
        self.due.push(Reverse((deadline, piece_index)));
    }

    fn pop_ready(&mut self, now: Instant) -> Option<u32> {
        self.due
            .peek()
            .is_some_and(|Reverse((deadline, _))| *deadline <= now)
            .then(|| self.due.pop().map(|Reverse((_, piece_index))| piece_index))
            .flatten()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.due.peek().map(|Reverse((deadline, _))| *deadline)
    }

    fn clear(&mut self) {
        self.due.clear();
        self.attempts.clear();
    }
}

async fn wait_for_uri_generation(
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    notifier: std::sync::Arc<tokio::sync::Notify>,
    observed: u64,
) {
    loop {
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if generation.load(std::sync::atomic::Ordering::Acquire) != observed {
            return;
        }
        notified.await;
    }
}

enum NoPeerWaitEvent {
    Peer(super::super::peer_events::PeerWaitEvent),
    WebSeed(WebSeedTaskCompletion),
    UriChanged,
}

type WebSeedTaskCompletion = Option<
    std::result::Result<(u32, std::result::Result<Vec<u8>, String>), tokio::task::JoinError>,
>;

enum PieceDownloadWait {
    Completed(PieceLoopAction),
    Incoming(Option<crate::engine::bt_peer_listener::IncomingPeer>),
    StopTimeout,
}

async fn wait_for_incoming_peer(
    receiver: Option<crate::engine::bt_peer_listener::IncomingPeerReceiver>,
) -> Option<crate::engine::bt_peer_listener::IncomingPeer> {
    match receiver {
        Some(receiver) => receiver.lock().await.recv().await,
        None => std::future::pending().await,
    }
}

impl PieceDownloadSession<'_> {
    pub(super) async fn run(mut self) -> Result<()> {
        let mut web_seed_tasks = tokio::task::JoinSet::new();
        let mut active_web_seed_pieces = std::collections::HashSet::new();
        let mut web_seed_retries = WebSeedRetries::default();
        let mut web_seed_scan_cursor = 0u32;
        let mut observed_uri_generation = self.command.group.recover().uri_generation();
        let web_seed_concurrency =
            self.command
                .group
                .recover()
                .options()
                .split
                .unwrap_or(crate::constants::DEFAULT_SPLIT)
                .clamp(1, MAX_WEB_SEED_PIECES_IN_FLIGHT as u16) as usize;
        self.announce_available_pieces().await;
        self.apply_upload_choke_round().await;
        loop {
            self.refresh_upload_stats();
            if !self.swarm.is_empty()
                && self
                    .command
                    .choking_algo
                    .as_ref()
                    .is_some_and(|algo| algo.choke_rotation_due(Instant::now()))
            {
                self.apply_upload_choke_round().await;
            }
            self.command
                .drain_incoming_peers_to_swarm(
                    self.swarm,
                    self.piece_length,
                    self.num_pieces,
                    self.total_size,
                    PeerActorUploadContext {
                        provider: std::sync::Arc::clone(&self.upload_provider),
                        upload_counter: std::sync::Arc::clone(&self.upload_counter),
                    },
                )
                .await;
            self.command.bt_runtime.set_connections(self.swarm.len());

            let (halt_requested, stop_timeout_elapsed) = {
                let group = self.command.group.recover();
                let halt_requested = group.is_force_halt_requested() || group.is_halt_requested();
                let stop_timeout_elapsed = !halt_requested
                    && self.stop_timeout.should_halt(
                        group.options().bt_stop_timeout,
                        self.command.completed_bytes,
                        Instant::now(),
                    );
                (halt_requested, stop_timeout_elapsed)
            };
            if stop_timeout_elapsed {
                let group = self.command.group.recover();
                let timeout_seconds = group.options().bt_stop_timeout.unwrap_or_default();
                warn!(
                    gid = group.gid().value(),
                    timeout_seconds,
                    "Stopping BitTorrent download after consecutive no-progress timeout"
                );
                group.request_force_halt(HaltReason::Timeout);
                group.set_last_error(DownloadResultCode::TimeOut, "Download timed out");
                continue;
            };
            if halt_requested {
                self.writer.flush().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to flush halted BT output: {error}"))
                })?;
                self.writer.close().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                })?;
                if let Some(checkpoint) = self.command.checkpoint.as_mut() {
                    let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(
                        &self.completed_bitfield,
                    );
                    checkpoint
                        .save(&bitfield, self.command.completed_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to save halted BT checkpoint: {error}"
                            ))
                        })?;
                    self.command
                        .group
                        .recover()
                        .take_save_control_file_request();
                }
                return Err(Aria2Error::DownloadFailed(
                    "BitTorrent download halted".into(),
                ));
            }

            let current_uri_generation = self.command.group.recover().uri_generation();
            if current_uri_generation != observed_uri_generation {
                observed_uri_generation = current_uri_generation;
                web_seed_scan_cursor = 0;
                web_seed_retries.clear();
            }
            self.schedule_web_seed_pieces(
                &mut web_seed_tasks,
                &mut active_web_seed_pieces,
                &mut web_seed_scan_cursor,
                &mut web_seed_retries,
                web_seed_concurrency,
            );
            while let Some(joined) = web_seed_tasks.try_join_next() {
                match joined {
                    Ok((piece_index, result)) => {
                        active_web_seed_pieces.remove(&piece_index);
                        if self.complete_web_seed_piece(piece_index, result).await? {
                            self.refresh_download_progress();
                        } else {
                            self.schedule_web_seed_retry(piece_index, &mut web_seed_retries);
                        }
                    }
                    Err(error) => {
                        warn!(%error, "WebSeed worker terminated unexpectedly");
                        web_seed_tasks.abort_all();
                        for piece_index in active_web_seed_pieces.drain() {
                            self.piece_picker.mark_reserved(piece_index, false);
                        }
                        break;
                    }
                }
            }

            if BtPieceSelector::is_complete(&self.piece_picker) {
                if self.endgame_state.is_endgame_active() {
                    self.endgame_state.exit_endgame();
                }
                break;
            }

            // Phase 14 - B1: Check if we should enter endgame mode
            let endgame_candidates = self.piece_picker.endgame_candidates();
            if !endgame_candidates.is_empty() && !self.endgame_state.is_endgame_active() {
                self.endgame_state.enter_endgame();
                info!(
                    "[BT] Endgame mode activated: {}/{} pieces remaining",
                    endgame_candidates.len(),
                    self.num_pieces
                );
            } else if endgame_candidates.is_empty() && self.endgame_state.is_endgame_active() {
                self.endgame_state.exit_endgame();
            }

            // G1: Periodic snub detection via extracted helper
            self.check_and_mark_swarm_peers_snubbed();
            {
                let group = self.command.group.recover();
                super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
            }

            // PEX Integration: Periodic PEX message sending (BEP 11)
            super::super::super::pex::send_periodic_pex_to_swarm(
                self.command,
                self.swarm,
                self.last_pex_send,
                self.pex_send_interval_secs,
            )
            .await;

            // PEX peers arrive as actor events during block reads or idle waits.
            // Connect only to endpoints not already owned by this Swarm.
            let all_new_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
                std::mem::take(&mut self.pending_pex_peers);
            if !all_new_pex_peers.is_empty() {
                for peer in &all_new_pex_peers {
                    self.command.add_pex_peer(peer.clone());
                }
                info!(
                    "[PEX] Drained {} inbound peers from connections, attempting to connect",
                    all_new_pex_peers.len()
                );
                // Attempt to connect to PEX-discovered peers
                let connected = self
                    .connect_to_discovered_swarm_peers(&all_new_pex_peers, BtPeerSource::Pex)
                    .await;
                if connected > 0 {
                    self.announce_available_pieces().await;
                    info!("[PEX] Successfully connected to {} new peers", connected);
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
                }
            }

            // Keep tracker numwant aligned with the live peer command count,
            // then re-announce when the tracker interval permits it.
            self.command.update_tracker_peer_state(self.swarm.len());
            if self.command.should_discover_more_peers(self.swarm.len()) {
                let new_peers = self
                    .command
                    .periodic_tracker_announce(
                        &self.meta.network_info_hash(),
                        self.command.completed_bytes,
                        self.total_size.saturating_sub(self.command.completed_bytes),
                        self.command.total_uploaded,
                    )
                    .await;
                if !new_peers.is_empty() {
                    info!(
                        "[BT] Periodic tracker announce found {} new peers",
                        new_peers.len()
                    );
                    for peer in &new_peers {
                        self.command.add_pex_peer(peer.clone());
                    }
                    // Connect to newly discovered peers
                    let connected = self
                        .connect_to_discovered_swarm_peers(&new_peers, BtPeerSource::Tracker)
                        .await;
                    if connected > 0 {
                        self.announce_available_pieces().await;
                        info!("[BT] Connected to {} new peers", connected);
                        let group = self.command.group.recover();
                        super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
                    }
                }
            }

            // DHTGetPeersCommand counterpart. The lookup runs in a background
            // task and publishes its result through an event slot, so DHT
            // timeouts do not stall piece scheduling or halt detection.
            self.command.dht_periodic_lookup.set_peer_limits(
                self.command.bt_runtime.min_peers(),
                self.command.bt_runtime.max_peers(),
            );
            let mut dht_peers = Vec::new();
            super::super::super::check_periodic_dht_lookup(
                &mut self.command.dht_periodic_lookup,
                self.command.dht_engine.as_ref(),
                &self.meta.network_info_hash(),
                self.swarm.len(),
                &mut dht_peers,
            )
            .await;
            dht_peers.retain(|peer| !self.command.is_peer_temporarily_rejected(&peer.ip));
            if !dht_peers.is_empty() {
                for peer in &dht_peers {
                    self.command.add_pex_peer(peer.clone());
                }
                info!(
                    discovered = dht_peers.len(),
                    "[BT] Periodic DHT lookup found new peers"
                );
                let connected = self
                    .connect_to_discovered_swarm_peers(&dht_peers, BtPeerSource::Dht)
                    .await;
                if connected > 0 {
                    self.announce_available_pieces().await;
                    info!("[BT] Connected to {} DHT-discovered peers", connected);
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
                }
            }
            if self
                .command
                .dht_periodic_lookup
                .is_lookup_completion_pending()
            {
                self.command
                    .dht_periodic_lookup
                    .on_lookup_completed(self.command.tracked_peer_count());
            }

            // With no connected peers, keep the torrent alive for tracker,
            // DHT, PEX, or incoming-peer discovery. The wait is driven by a
            // socket/message event, a lifecycle notification, a completed DHT
            // lookup, or the next protocol/stop-timeout deadline.
            if self.swarm.is_empty() {
                debug!("[BT] No peers available, waiting for peer discovery...");
                let peer_deadline = self
                    .command
                    .next_peer_event_deadline(self.swarm.len(), self.stop_timeout.deadline());
                let protocol_deadline = web_seed_retries
                    .next_deadline()
                    .map_or(peer_deadline, |retry_deadline| {
                        peer_deadline.min(retry_deadline)
                    });
                let deadline = protocol_deadline.min(self.next_snub_check_deadline());
                let (uri_generation, uri_notifier) = {
                    let group = self.command.group.recover();
                    (group.uri_generation_handle(), group.uri_notifier())
                };
                let peer_event = self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                let event = tokio::select! {
                    event = peer_event => NoPeerWaitEvent::Peer(event),
                    joined = web_seed_tasks.join_next(), if !web_seed_tasks.is_empty() => {
                        NoPeerWaitEvent::WebSeed(joined)
                    }
                    _ = wait_for_uri_generation(
                        uri_generation,
                        uri_notifier,
                        observed_uri_generation,
                    ) => NoPeerWaitEvent::UriChanged,
                };
                let event = match event {
                    NoPeerWaitEvent::WebSeed(Some(Ok((piece_index, result)))) => {
                        active_web_seed_pieces.remove(&piece_index);
                        if self.complete_web_seed_piece(piece_index, result).await? {
                            self.refresh_download_progress();
                        } else {
                            self.schedule_web_seed_retry(piece_index, &mut web_seed_retries);
                        }
                        continue;
                    }
                    NoPeerWaitEvent::WebSeed(Some(Err(error))) => {
                        warn!(%error, "WebSeed worker terminated unexpectedly");
                        web_seed_tasks.abort_all();
                        for piece_index in active_web_seed_pieces.drain() {
                            self.piece_picker.mark_reserved(piece_index, false);
                        }
                        continue;
                    }
                    NoPeerWaitEvent::WebSeed(None) | NoPeerWaitEvent::UriChanged => continue,
                    NoPeerWaitEvent::Peer(event) => event,
                };
                let actor_interest_changed = match &event {
                    super::super::peer_events::PeerWaitEvent::Actor(actor_event) => {
                        self.apply_swarm_peer_event(actor_event)
                    }
                    _ => false,
                };
                let interest_changed = actor_interest_changed;
                let incoming = match event {
                    super::super::peer_events::PeerWaitEvent::Incoming(incoming) => Some(incoming),
                    _ => None,
                };
                if let Some(incoming) = incoming {
                    self.command.admit_incoming_peer_to_swarm(
                        self.swarm,
                        incoming,
                        self.piece_length,
                        self.num_pieces,
                        self.total_size,
                        &PeerActorUploadContext {
                            provider: std::sync::Arc::clone(&self.upload_provider),
                            upload_counter: std::sync::Arc::clone(&self.upload_counter),
                        },
                    );
                    self.announce_available_pieces().await;
                }
                if interest_changed {
                    self.apply_upload_choke_round().await;
                }
                self.remove_dead_swarm_peers().await;
                continue;
            }

            let remaining = self.piece_picker.remaining_count();
            let selection = self
                .piece_selector
                .select_next_piece(&mut self.piece_picker, remaining);

            let next_piece_idx = match selection.piece_index {
                Some(idx) => idx,
                None => {
                    if !web_seed_tasks.is_empty() {
                        match web_seed_tasks.join_next().await {
                            Some(Ok((piece_index, result))) => {
                                active_web_seed_pieces.remove(&piece_index);
                                if self.complete_web_seed_piece(piece_index, result).await? {
                                    self.refresh_download_progress();
                                } else {
                                    self.schedule_web_seed_retry(
                                        piece_index,
                                        &mut web_seed_retries,
                                    );
                                }
                            }
                            Some(Err(error)) => {
                                warn!(%error, "WebSeed worker terminated unexpectedly");
                                web_seed_tasks.abort_all();
                                for piece_index in active_web_seed_pieces.drain() {
                                    self.piece_picker.mark_reserved(piece_index, false);
                                }
                            }
                            None => {}
                        }
                        continue;
                    }
                    tracing::debug!("[BT] No piece available, waiting...");
                    let peer_deadline = self
                        .command
                        .next_peer_event_deadline(self.swarm.len(), self.stop_timeout.deadline());
                    let protocol_deadline = web_seed_retries
                        .next_deadline()
                        .map_or(peer_deadline, |retry_deadline| {
                            peer_deadline.min(retry_deadline)
                        });
                    let choke_deadline = (!self.swarm.is_empty())
                        .then(|| {
                            self.command
                                .choking_algo
                                .as_ref()
                                .and_then(|algo| algo.next_choke_rotation_deadline())
                        })
                        .flatten();
                    let deadline = choke_deadline
                        .map_or(protocol_deadline, |choke_deadline| {
                            protocol_deadline.min(choke_deadline)
                        })
                        .min(self.next_snub_check_deadline());
                    let event = self
                        .command
                        .wait_for_swarm_peer_event(self.swarm, deadline)
                        .await;
                    let actor_interest_changed = match &event {
                        super::super::peer_events::PeerWaitEvent::Actor(actor_event) => {
                            self.apply_swarm_peer_event(actor_event)
                        }
                        _ => false,
                    };
                    let interest_changed = actor_interest_changed;
                    let incoming = match event {
                        super::super::peer_events::PeerWaitEvent::Incoming(incoming) => {
                            Some(incoming)
                        }
                        _ => None,
                    };
                    if let Some(incoming) = incoming {
                        self.command.admit_incoming_peer_to_swarm(
                            self.swarm,
                            incoming,
                            self.piece_length,
                            self.num_pieces,
                            self.total_size,
                            &PeerActorUploadContext {
                                provider: std::sync::Arc::clone(&self.upload_provider),
                                upload_counter: std::sync::Arc::clone(&self.upload_counter),
                            },
                        );
                        self.announce_available_pieces().await;
                    }
                    if interest_changed {
                        self.apply_upload_choke_round().await;
                    }
                    self.remove_dead_swarm_peers().await;
                    continue;
                }
            };

            self.piece_picker
                .mark_in_progress(next_piece_idx as u32, true);
            let incoming_receiver = self.command.incoming_peers.clone();
            let wait = if let Some(deadline) = self.stop_timeout.deadline() {
                tokio::select! {
                    action = self.download_piece(next_piece_idx) => PieceDownloadWait::Completed(action?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming),
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        PieceDownloadWait::StopTimeout
                    }
                }
            } else {
                tokio::select! {
                    action = self.download_piece(next_piece_idx) => PieceDownloadWait::Completed(action?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming),
                }
            };
            let action = match wait {
                PieceDownloadWait::Completed(action) => action,
                PieceDownloadWait::Incoming(Some(incoming)) => {
                    self.command.admit_incoming_peer_to_swarm(
                        self.swarm,
                        incoming,
                        self.piece_length,
                        self.num_pieces,
                        self.total_size,
                        &PeerActorUploadContext {
                            provider: std::sync::Arc::clone(&self.upload_provider),
                            upload_counter: std::sync::Arc::clone(&self.upload_counter),
                        },
                    );
                    self.announce_available_pieces().await;
                    self.piece_picker
                        .mark_in_progress(next_piece_idx as u32, false);
                    continue;
                }
                PieceDownloadWait::Incoming(None) => {
                    self.command.incoming_peers = None;
                    self.piece_picker
                        .mark_in_progress(next_piece_idx as u32, false);
                    continue;
                }
                PieceDownloadWait::StopTimeout => {
                    self.piece_picker
                        .mark_in_progress(next_piece_idx as u32, false);
                    continue;
                }
            };
            if matches!(action, PieceLoopAction::Retry) {
                self.piece_picker
                    .mark_in_progress(next_piece_idx as u32, false);
            }
            if matches!(action, PieceLoopAction::RefreshProgress) {
                self.refresh_download_progress();
            }
        }
        tracing::info!("[BT] Finalizing writer...");
        self.writer
            .flush()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to flush BT output: {error}")))?;
        self.writer
            .close()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to close BT output: {error}")))?;
        if let Some(checkpoint) = self.command.checkpoint.take() {
            checkpoint.remove().await?;
        }
        tracing::info!("[BT] Writer flushed and closed OK");
        info!(
            "BT download done: {} ({} bytes)",
            self.command.output_path.display(),
            self.command.completed_bytes
        );

        Ok(())
    }

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
                    if self
                        .peer_tracker
                        .update_peer_piece(&peer, *piece_index, *has_piece)
                    {
                        self.peer_last_data_time
                            .insert(PeerKey::new(actor.endpoint), Instant::now());
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
                    self.peer_last_data_time
                        .insert(PeerKey::new(actor.endpoint), Instant::now());
                }
                false
            }
            PeerEvent::PexPeers { peers } => {
                self.pending_pex_peers.extend(peers.iter().cloned());
                false
            }
            PeerEvent::AllowedFast { .. } => false,
            PeerEvent::PexNegotiated { .. } => false,
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
            | PeerEvent::RequestFailed { .. }
            | PeerEvent::AvailabilityChanged { .. }
            | PeerEvent::Message { .. } => false,
        }
    }

    pub(super) async fn remove_dead_swarm_peers(&mut self) {
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
            return;
        }

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

        let removed = self.swarm.remove_dead().await;
        let mut peer_storage = self
            .command
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, endpoint) in removed {
            peer_storage.return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
        }
    }

    fn schedule_web_seed_pieces(
        &mut self,
        tasks: &mut tokio::task::JoinSet<(u32, std::result::Result<Vec<u8>, String>)>,
        active_pieces: &mut std::collections::HashSet<u32>,
        scan_cursor: &mut u32,
        retries: &mut WebSeedRetries,
        concurrency: usize,
    ) {
        let Some(manager) = self.web_seed_manager.as_ref() else {
            return;
        };
        while tasks.len() < concurrency {
            let piece_index = match retries.pop_ready(Instant::now()) {
                Some(piece_index) => piece_index,
                None if *scan_cursor < self.num_pieces => {
                    let piece_index = *scan_cursor;
                    *scan_cursor += 1;
                    piece_index
                }
                None => break,
            };
            if !self.piece_picker.is_allowed(piece_index)
                || self.piece_picker.is_completed(piece_index)
                || self.piece_picker.is_in_progress(piece_index)
                || self.piece_picker.is_reserved(piece_index)
            {
                continue;
            }
            let piece_data_length = self.actual_piece_length(piece_index as usize);
            if piece_data_length == 0
                || !manager.has_complete_sources_for_piece(piece_index, piece_data_length)
            {
                continue;
            }

            self.piece_picker.mark_reserved(piece_index, true);
            active_pieces.insert(piece_index);
            let manager = std::sync::Arc::clone(manager);
            let group = std::sync::Arc::clone(&self.command.group);
            let progress = std::sync::Arc::clone(&self.command.progress);
            tasks.spawn(async move {
                let connection = ActiveConnectionGuard::new(group);
                connection.set(1);
                let result = manager
                    .request_piece_with_length_and_activity(
                        piece_index,
                        piece_data_length as u64,
                        Some(progress.as_ref()),
                    )
                    .await;
                (piece_index, result)
            });
        }
    }

    fn schedule_web_seed_retry(&self, piece_index: u32, retries: &mut WebSeedRetries) {
        let group = self.command.group.recover();
        retries.schedule(
            piece_index,
            group.options().max_retries,
            Duration::from_secs(group.options().retry_wait),
        );
    }

    fn refresh_download_progress(&mut self) {
        self.command
            .progress
            .set_completed_length(self.command.completed_bytes);

        let elapsed = self.last_speed_update.elapsed();
        if elapsed.as_millis() >= 500 {
            let delta = self
                .command
                .completed_bytes
                .saturating_sub(self.last_completed);
            let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
            self.command.progress.set_download_speed(speed);
            self.last_speed_update = Instant::now();
            self.last_completed = self.command.completed_bytes;
        }
        self.refresh_upload_stats();
    }

    fn refresh_upload_stats(&mut self) {
        let uploaded_by_peers = self
            .upload_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let delta = uploaded_by_peers.saturating_sub(self.last_uploaded);
        if delta > 0 {
            self.command.total_uploaded = self.command.total_uploaded.saturating_add(delta);
            self.command
                .progress
                .set_upload_length(self.command.total_uploaded);
            self.last_uploaded = uploaded_by_peers;
        }

        let elapsed = self.last_upload_speed_update.elapsed();
        if elapsed.as_millis() >= 500 {
            let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
            self.command.progress.set_upload_speed(speed);
            self.last_upload_speed_update = Instant::now();
        }
    }
}

impl PieceDownloadSession<'_> {
    async fn connect_to_discovered_swarm_peers(
        &mut self,
        peers: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        source: BtPeerSource,
    ) -> usize {
        let active_peers = self
            .swarm
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| (actor.endpoint.ip().to_string(), actor.endpoint.port()))
            .collect::<std::collections::HashSet<_>>();
        let candidates = self
            .command
            .peer_coordinator
            .select_candidates(peers, &active_peers);
        if candidates.is_empty() {
            return 0;
        }
        let info_hash = self.meta.network_info_hash();
        let connections = self
            .command
            .connect_to_discovered_peers(
                &candidates,
                source,
                &info_hash,
                self.num_pieces,
                self.piece_length,
                self.total_size,
            )
            .await;
        self.admit_connected_peers(connections)
    }

    pub(super) async fn apply_upload_choke_round(&mut self) {
        self.command
            .apply_upload_choke_round_swarm(self.swarm)
            .await;
    }

    fn admit_connected_peers(&mut self, new_connections: Vec<BtPeerConn>) -> usize {
        let max_peers = self.command.group.recover().options().bt_max_peers;
        let caretaker_id = self.command.group.recover().gid().value();
        let mut seen_endpoints = std::collections::HashSet::with_capacity(new_connections.len());
        let dht_engine = self.command.dht_engine.clone();
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

            let entry = crate::engine::bt_peer_storage::PeerEntry::new(
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
            connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
            connection.set_upload_progress(std::sync::Arc::clone(&self.command.progress));
            let peer_key = PeerKey::new(endpoint);
            let stats = connection.stats.clone();

            if self
                .swarm
                .spawn_peer(
                    connection,
                    dht_engine.clone(),
                    std::sync::Arc::clone(&provider),
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

    async fn announce_available_pieces(&mut self) {
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

    fn check_and_mark_swarm_peers_snubbed(&mut self) {
        const CHECK_INTERVAL: Duration = Duration::from_secs(10);
        if self.last_snub_check.elapsed() < CHECK_INTERVAL {
            return;
        }
        self.last_snub_check = Instant::now();
        let timeout = self
            .command
            .group
            .recover()
            .options()
            .bt_snubbed_timeout
            .unwrap_or(60);
        for actor in self.swarm.iter_mut().filter(|actor| !actor.dead) {
            if actor.stats.check_snubbed(timeout) {
                debug!(peer = %actor.endpoint, timeout, "Marked peer actor as snubbed");
            }
        }
    }

    fn next_snub_check_deadline(&self) -> Instant {
        self.last_snub_check
            .checked_add(Duration::from_secs(10))
            .unwrap_or_else(Instant::now)
    }
}

#[cfg(test)]
mod tests {
    use super::WebSeedRetries;
    use std::time::{Duration, Instant};

    #[test]
    fn web_seed_retries_are_delayed_and_limited() {
        let now = Instant::now();
        let mut delayed = WebSeedRetries::default();
        delayed.schedule(7, 2, Duration::from_secs(60));
        assert_eq!(delayed.pop_ready(now), None);
        assert!(delayed.next_deadline().is_some());

        let mut retries = WebSeedRetries::default();
        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), Some(7));

        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), Some(7));

        retries.schedule(7, 2, Duration::ZERO);
        assert_eq!(retries.pop_ready(Instant::now()), None);
    }
}
