use std::time::Instant;
use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};

use crate::engine::bittorrent::download::execute::types::PeerKey;
use crate::engine::bittorrent::peer::choking_algorithm::PeerIdentity;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::message_handler::types::BLOCK_SIZE;
use crate::engine::bittorrent::peer::message_handler::{PeerCommand, PeerEvent};
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    ActiveConnectionGuard, BtPeerSource, DownloadResultCode, HaltReason,
};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

use super::peer_dials::{PeerDialConfig, PeerDialQueue};
use super::{PieceDownloadSession, PieceLoopAction};

const MAX_WEB_SEED_PIECES_IN_FLIGHT: usize = 4;
const MAX_BT_PIECES_IN_FLIGHT: usize = 8;
const MAX_BT_PIECE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

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
    PeerDial(Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>>),
    WebSeed(WebSeedTaskCompletion),
    UriChanged,
}

type WebSeedTaskCompletion = Option<
    std::result::Result<(u32, std::result::Result<Vec<u8>, String>), tokio::task::JoinError>,
>;

enum PieceDownloadWait {
    Completed(Vec<(usize, PieceLoopAction)>),
    Incoming(Option<Box<crate::engine::bittorrent::peer::listener::IncomingPeer>>),
    PeerDial(Option<std::result::Result<Vec<BtPeerConn>, tokio::task::JoinError>>),
    StopTimeout,
}

async fn wait_for_incoming_peer(
    receiver: Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
) -> Option<crate::engine::bittorrent::peer::listener::IncomingPeer> {
    match receiver {
        Some(receiver) => receiver.lock().await.recv().await,
        None => std::future::pending().await,
    }
}

impl PieceDownloadSession<'_> {
    fn normal_piece_batch_limit(&self, first_piece_index: usize) -> usize {
        if self.endgame_state.is_endgame_active() {
            return 1;
        }
        let piece_length = self.actual_piece_length(first_piece_index).max(1) as usize;
        let blocks_per_piece = piece_length.div_ceil(BLOCK_SIZE as usize).max(1);
        let aggregate_request_window = self
            .swarm
            .iter()
            .filter(|peer| !peer.dead)
            .map(|peer| peer.max_outstanding_requests)
            .sum::<usize>();
        let window_limited = aggregate_request_window.div_ceil(blocks_per_piece).max(1);
        let memory_limited = (MAX_BT_PIECE_BUFFER_BYTES / piece_length).max(1);
        window_limited
            .min(MAX_BT_PIECES_IN_FLIGHT)
            .min(memory_limited)
    }

    pub(super) async fn run(self) -> Result<()> {
        let upload_speed_reporter =
            crate::engine::bittorrent::download::execute::spawn_upload_speed_reporter(
                std::sync::Arc::clone(&self.command.progress),
                self.swarm.upload_rate(),
            );
        let result = self.run_loop().await;
        upload_speed_reporter.abort();
        let _ = upload_speed_reporter.await;
        result
    }

    async fn run_loop(mut self) -> Result<()> {
        let mut peer_dials = PeerDialQueue::default();
        let peer_dial_config = PeerDialConfig::new(
            self.command,
            self.meta.network_info_hash(),
            self.meta.info_hash_v2,
            self.num_pieces,
            self.piece_length,
            self.total_size,
        );
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
        self.apply_upload_choke_round();
        loop {
            if let Some(result) = peer_dials.try_join_next() {
                self.handle_peer_dial_batch_result(result).await;
            }
            self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
            self.refresh_upload_stats();
            if !self.swarm.is_empty()
                && self
                    .command
                    .choking_algo
                    .as_ref()
                    .is_some_and(|algo| algo.choke_rotation_due(Instant::now()))
            {
                self.apply_upload_choke_round();
            }
            self.command
                .drain_incoming_peers_to_swarm(self.swarm, self.peer_actor_admission_context())
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
                        .save_with_in_flight_pieces(
                            &bitfield,
                            self.command.completed_bytes,
                            &super::piece::in_flight_snapshot(&self.in_flight_pieces),
                        )
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

            // Tracker results use the same Swarm event stream even while there
            // is no connected peer or a piece batch is waiting on a response.
            // Admit them at the next coordinator turn before entering an idle
            // wait, so an empty swarm cannot strand discovered endpoints.
            let tracker_peers = std::mem::take(&mut self.pending_tracker_peers);
            if !tracker_peers.is_empty() {
                self.queue_discovered_swarm_peers(
                    &tracker_peers,
                    BtPeerSource::Tracker,
                    &mut peer_dials,
                    &peer_dial_config,
                );
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
                self.swarm.set_local_seeder(true);
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
                self.swarm,
                self.last_pex_send,
                self.command.peer_exchange_enabled(),
            )
            .await;

            // PEX peers arrive as actor events during block reads or idle waits.
            // Connect only to endpoints not already owned by this Swarm.
            let all_new_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
                std::mem::take(&mut self.pending_pex_peers);
            if !all_new_pex_peers.is_empty() {
                info!(
                    "[PEX] Drained {} inbound peers from connections, attempting to connect",
                    all_new_pex_peers.len()
                );
                // Attempt to connect to PEX-discovered peers
                self.queue_discovered_swarm_peers(
                    &all_new_pex_peers,
                    BtPeerSource::Pex,
                    &mut peer_dials,
                    &peer_dial_config,
                );
            }

            // The tracker actor reads this live count when its announce
            // deadline fires; no piece-loop timer or network I/O is needed.
            self.command.update_tracker_peer_state(self.swarm.len());

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
                &self.command.dht_engines,
                &self.meta.network_info_hash(),
                self.command.listen_port,
                self.swarm.len(),
                &mut dht_peers,
            )
            .await;
            dht_peers.retain(|peer| !self.command.is_peer_temporarily_rejected(&peer.ip));
            if !dht_peers.is_empty() {
                info!(
                    discovered = dht_peers.len(),
                    "[BT] Periodic DHT lookup found new peers"
                );
                self.queue_discovered_swarm_peers(
                    &dht_peers,
                    BtPeerSource::Dht,
                    &mut peer_dials,
                    &peer_dial_config,
                );
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
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        NoPeerWaitEvent::PeerDial(joined)
                    }
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
                    NoPeerWaitEvent::PeerDial(Some(result)) => {
                        self.handle_peer_dial_batch_result(result).await;
                        self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                        continue;
                    }
                    NoPeerWaitEvent::PeerDial(None) => continue,
                    NoPeerWaitEvent::WebSeed(None) | NoPeerWaitEvent::UriChanged => continue,
                    NoPeerWaitEvent::Peer(event) => event,
                };
                self.handle_peer_wait_event(event).await;
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
                        let peer_deadline = self.command.next_peer_event_deadline(
                            self.swarm.len(),
                            self.stop_timeout.deadline(),
                        );
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
                        let (uri_generation, uri_notifier) = {
                            let group = self.command.group.recover();
                            (group.uri_generation_handle(), group.uri_notifier())
                        };
                        let peer_event =
                            self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                        let uri_changed = wait_for_uri_generation(
                            uri_generation,
                            uri_notifier,
                            observed_uri_generation,
                        );
                        let event = tokio::select! {
                            event = peer_event => NoPeerWaitEvent::Peer(event),
                            joined = web_seed_tasks.join_next() => NoPeerWaitEvent::WebSeed(joined),
                            joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                                NoPeerWaitEvent::PeerDial(joined)
                            },
                            _ = uri_changed => NoPeerWaitEvent::UriChanged,
                        };
                        match event {
                            NoPeerWaitEvent::WebSeed(Some(Ok((piece_index, result)))) => {
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
                            NoPeerWaitEvent::WebSeed(Some(Err(error))) => {
                                warn!(%error, "WebSeed worker terminated unexpectedly");
                                web_seed_tasks.abort_all();
                                for piece_index in active_web_seed_pieces.drain() {
                                    self.piece_picker.mark_reserved(piece_index, false);
                                }
                            }
                            NoPeerWaitEvent::WebSeed(None) => {}
                            NoPeerWaitEvent::PeerDial(Some(result)) => {
                                self.handle_peer_dial_batch_result(result).await;
                                self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                            }
                            NoPeerWaitEvent::PeerDial(None) | NoPeerWaitEvent::UriChanged => {}
                            NoPeerWaitEvent::Peer(event) => {
                                self.handle_peer_wait_event(event).await;
                            }
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
                    let peer_event = self.command.wait_for_swarm_peer_event(self.swarm, deadline);
                    let event = tokio::select! {
                        event = peer_event => NoPeerWaitEvent::Peer(event),
                        joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                            NoPeerWaitEvent::PeerDial(joined)
                        }
                    };
                    let event = match event {
                        NoPeerWaitEvent::PeerDial(Some(result)) => {
                            self.handle_peer_dial_batch_result(result).await;
                            self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                            continue;
                        }
                        NoPeerWaitEvent::PeerDial(None) => continue,
                        NoPeerWaitEvent::Peer(event) => event,
                        NoPeerWaitEvent::WebSeed(_) | NoPeerWaitEvent::UriChanged => continue,
                    };
                    self.handle_peer_wait_event(event).await;
                    continue;
                }
            };

            let batch_limit = self.normal_piece_batch_limit(next_piece_idx);
            let mut selected_pieces = vec![next_piece_idx];
            self.piece_picker
                .mark_in_progress(next_piece_idx as u32, true);
            while selected_pieces.len() < batch_limit {
                let remaining = self.piece_picker.remaining_count();
                let next = self
                    .piece_selector
                    .select_next_piece(&mut self.piece_picker, remaining)
                    .piece_index;
                let Some(next) = next else {
                    break;
                };
                if selected_pieces.contains(&next) {
                    break;
                }
                self.piece_picker.mark_in_progress(next as u32, true);
                selected_pieces.push(next);
            }
            let incoming_receiver = self.command.incoming_peers.clone();
            let wait = if let Some(deadline) = self.stop_timeout.deadline() {
                tokio::select! {
                    actions = self.download_piece_batch(&selected_pieces) => PieceDownloadWait::Completed(actions?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming.map(Box::new)),
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        PieceDownloadWait::PeerDial(joined)
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        PieceDownloadWait::StopTimeout
                    }
                }
            } else {
                tokio::select! {
                    actions = self.download_piece_batch(&selected_pieces) => PieceDownloadWait::Completed(actions?),
                    incoming = wait_for_incoming_peer(incoming_receiver) => PieceDownloadWait::Incoming(incoming.map(Box::new)),
                    joined = peer_dials.join_next(), if peer_dials.has_active_batch() => {
                        PieceDownloadWait::PeerDial(joined)
                    }
                }
            };
            let action = match wait {
                PieceDownloadWait::Completed(action) => action,
                PieceDownloadWait::Incoming(Some(incoming)) => {
                    let context = self.peer_actor_admission_context();
                    self.command
                        .admit_incoming_peer_to_swarm(self.swarm, *incoming, &context);
                    self.announce_available_pieces().await;
                    for piece_index in &selected_pieces {
                        self.piece_picker
                            .mark_in_progress(*piece_index as u32, false);
                    }
                    continue;
                }
                PieceDownloadWait::Incoming(None) => {
                    self.command.incoming_peers = None;
                    for piece_index in &selected_pieces {
                        self.piece_picker
                            .mark_in_progress(*piece_index as u32, false);
                    }
                    continue;
                }
                PieceDownloadWait::PeerDial(Some(result)) => {
                    self.handle_peer_dial_batch_result(result).await;
                    self.start_next_peer_dial_batch(&mut peer_dials, &peer_dial_config);
                    for piece_index in &selected_pieces {
                        self.piece_picker
                            .mark_in_progress(*piece_index as u32, false);
                    }
                    continue;
                }
                PieceDownloadWait::PeerDial(None) => continue,
                PieceDownloadWait::StopTimeout => {
                    for piece_index in &selected_pieces {
                        self.piece_picker
                            .mark_in_progress(*piece_index as u32, false);
                    }
                    continue;
                }
            };
            let mut refresh_progress = false;
            for (piece_index, action) in action {
                match action {
                    PieceLoopAction::Retry => {
                        self.piece_picker
                            .mark_in_progress(piece_index as u32, false);
                    }
                    PieceLoopAction::RefreshProgress => refresh_progress = true,
                }
            }
            if refresh_progress {
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

        self.command
            .progress
            .set_upload_speed(self.swarm.upload_speed_at(Instant::now()));
    }
}

impl PieceDownloadSession<'_> {
    async fn handle_peer_wait_event(&mut self, event: super::super::peer_events::PeerWaitEvent) {
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

    fn queue_discovered_swarm_peers(
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

    fn start_next_peer_dial_batch(
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

    async fn handle_peer_dial_batch_result(
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
            connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
            connection.set_upload_progress(std::sync::Arc::clone(&self.command.progress));
            let peer_key = PeerKey::new(endpoint);
            let stats = connection.stats.clone();

            if self
                .swarm
                .spawn_peer(
                    connection,
                    self.command.dht_engines.for_peer(endpoint),
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
