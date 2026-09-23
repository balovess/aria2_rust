use std::time::Instant;
use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    ActiveConnectionGuard, BtPeerSource, DownloadResultCode, HaltReason,
};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

use super::super::peer_events::NewPeerConnectionsContext;
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
                .unwrap_or(crate::constants::DEFAULT_SPLIT as u16)
                .clamp(1, MAX_WEB_SEED_PIECES_IN_FLIGHT as u16) as usize;
        self.announce_available_pieces().await;
        self.command
            .apply_upload_choke_round(self.active_connections)
            .await;
        self.last_upload_choke_round = Instant::now();
        loop {
            self.refresh_upload_stats();
            let choke_interval = self
                .command
                .choking_algo
                .as_ref()
                .map(|algo| algo.config().choke_rotation_interval_secs)
                .unwrap_or(0);
            if choke_interval > 0
                && self.last_upload_choke_round.elapsed().as_secs() >= choke_interval
            {
                self.command
                    .apply_upload_choke_round(self.active_connections)
                    .await;
                self.last_upload_choke_round = Instant::now();
            }
            self.command.drain_incoming_peers(
                self.active_connections,
                self.piece_length,
                self.num_pieces,
                self.total_size,
            );
            self.command
                .bt_runtime
                .set_connections(self.active_connections.len());

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
            self.command.check_and_mark_snubbed_peers(
                &mut self.last_snub_check,
                &self.peer_last_data_time,
                self.active_connections,
            );
            {
                let group = self.command.group.recover();
                super::super::sync_peer_snapshots(&group, self.active_connections);
            }

            // PEX Integration: Periodic PEX message sending (BEP 11)
            super::super::super::pex::send_periodic_pex(
                self.command,
                self.active_connections,
                self.pex_enabled_peers,
                self.last_pex_send,
                self.pex_send_interval_secs,
            )
            .await;

            // PEX Integration: Drain inbound PEX peers from all connections.
            // Peers are accumulated during block reads and stashed on
            // BtPeerConn::pending_pex_peers. Here we drain them and add to
            // our known-peers list.
            let mut all_new_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
                Vec::new();
            for conn in self.active_connections.iter_mut() {
                let peers = conn.drain_pex_peers();
                if !peers.is_empty() {
                    for peer in &peers {
                        self.command.add_pex_peer(peer.clone());
                    }
                    all_new_pex_peers.extend(peers);
                }
            }
            if !all_new_pex_peers.is_empty() {
                info!(
                    "[PEX] Drained {} inbound peers from connections, attempting to connect",
                    all_new_pex_peers.len()
                );
                // Attempt to connect to PEX-discovered peers
                let new_connections = self
                    .command
                    .connect_to_discovered_peers(
                        &all_new_pex_peers,
                        BtPeerSource::Pex,
                        &self.meta.network_info_hash(),
                        self.num_pieces,
                        self.active_connections,
                        self.piece_length,
                        self.total_size,
                    )
                    .await;
                let connected = self.append_new_connections(new_connections);
                if connected > 0 {
                    self.announce_available_pieces().await;
                    info!("[PEX] Successfully connected to {} new peers", connected);
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots(&group, self.active_connections);
                }
            }

            // Keep tracker numwant aligned with the live peer command count,
            // then re-announce when the tracker interval permits it.
            self.command
                .update_tracker_peer_state(self.active_connections.len());
            if self
                .command
                .should_discover_more_peers(self.active_connections.len())
            {
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
                    // Connect to newly discovered peers
                    let new_connections = self
                        .command
                        .connect_to_discovered_peers(
                            &new_peers,
                            BtPeerSource::Tracker,
                            &self.meta.network_info_hash(),
                            self.num_pieces,
                            self.active_connections,
                            self.piece_length,
                            self.total_size,
                        )
                        .await;
                    let connected = self.append_new_connections(new_connections);
                    if connected > 0 {
                        self.announce_available_pieces().await;
                        info!("[BT] Connected to {} new peers", connected);
                        let group = self.command.group.recover();
                        super::super::sync_peer_snapshots(&group, self.active_connections);
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
                self.active_connections.len(),
                &mut dht_peers,
            )
            .await;
            dht_peers.retain(|peer| !self.command.is_peer_temporarily_rejected(&peer.ip));
            if !dht_peers.is_empty() {
                info!(
                    discovered = dht_peers.len(),
                    "[BT] Periodic DHT lookup found new peers"
                );
                let new_connections = self
                    .command
                    .connect_to_discovered_peers(
                        &dht_peers,
                        BtPeerSource::Dht,
                        &self.meta.network_info_hash(),
                        self.num_pieces,
                        self.active_connections,
                        self.piece_length,
                        self.total_size,
                    )
                    .await;
                let connected = self.append_new_connections(new_connections);
                if connected > 0 {
                    self.announce_available_pieces().await;
                    info!("[BT] Connected to {} DHT-discovered peers", connected);
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots(&group, self.active_connections);
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
            if self.active_connections.is_empty()
                && web_seed_tasks.is_empty()
                && (self
                    .web_seed_manager
                    .as_ref()
                    .is_none_or(|manager| manager.is_empty())
                    || web_seed_scan_cursor >= self.num_pieces)
            {
                debug!("[BT] No peers available, waiting for peer discovery...");
                let peer_deadline = self.command.next_peer_event_deadline(
                    self.active_connections,
                    self.stop_timeout.deadline(),
                );
                let deadline = web_seed_retries
                    .next_deadline()
                    .map_or(peer_deadline, |retry_deadline| {
                        peer_deadline.min(retry_deadline)
                    });
                let (uri_generation, uri_notifier) = {
                    let group = self.command.group.recover();
                    (group.uri_generation_handle(), group.uri_notifier())
                };
                let peer_event = self.command.wait_for_peer_event(
                    self.active_connections,
                    deadline,
                    Some(std::sync::Arc::clone(&self.upload_provider)),
                );
                let event = tokio::select! {
                    event = peer_event => Some(event),
                    _ = wait_for_uri_generation(
                        uri_generation,
                        uri_notifier,
                        observed_uri_generation,
                    ) => None,
                };
                let Some(event) = event else {
                    continue;
                };
                let incoming = BtDownloadCommand::apply_peer_wait_event(
                    event,
                    self.active_connections,
                    &mut self.peer_tracker,
                    self.pex_enabled_peers,
                    &mut self.peer_last_data_time,
                    &mut self.command.allowed_fast_sent_peers,
                    &mut self.command.suggest_sent_counts,
                    &mut self.endgame_state,
                    self.command.choking_algo.as_mut(),
                    &self.command.peer_storage,
                );
                if let Some(incoming) = incoming {
                    self.command.admit_incoming_peer(
                        self.active_connections,
                        incoming,
                        self.piece_length,
                        self.num_pieces,
                        self.total_size,
                    );
                    self.announce_available_pieces().await;
                }
                BtDownloadCommand::send_due_keepalives(self.active_connections).await;
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
                    let peer_deadline = self.command.next_peer_event_deadline(
                        self.active_connections,
                        self.stop_timeout.deadline(),
                    );
                    let deadline = web_seed_retries
                        .next_deadline()
                        .map_or(peer_deadline, |retry_deadline| {
                            peer_deadline.min(retry_deadline)
                        });
                    let event = self
                        .command
                        .wait_for_peer_event(
                            self.active_connections,
                            deadline,
                            Some(std::sync::Arc::clone(&self.upload_provider)),
                        )
                        .await;
                    let incoming = BtDownloadCommand::apply_peer_wait_event(
                        event,
                        self.active_connections,
                        &mut self.peer_tracker,
                        self.pex_enabled_peers,
                        &mut self.peer_last_data_time,
                        &mut self.command.allowed_fast_sent_peers,
                        &mut self.command.suggest_sent_counts,
                        &mut self.endgame_state,
                        self.command.choking_algo.as_mut(),
                        &self.command.peer_storage,
                    );
                    if let Some(incoming) = incoming {
                        self.command.admit_incoming_peer(
                            self.active_connections,
                            incoming,
                            self.piece_length,
                            self.num_pieces,
                            self.total_size,
                        );
                        self.announce_available_pieces().await;
                    }
                    BtDownloadCommand::send_due_keepalives(self.active_connections).await;
                    continue;
                }
            };

            self.piece_picker
                .mark_in_progress(next_piece_idx as u32, true);
            let action = self.download_piece(next_piece_idx).await?;
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

impl PieceDownloadSession<'_> {
    fn append_new_connections(&mut self, new_connections: Vec<BtPeerConn>) -> usize {
        let mut new_connections = new_connections;
        let max_peers = self.command.group.recover().options().bt_max_peers;
        let caretaker_id = self.command.group.recover().gid().value();
        let is_private = self.command.is_private;
        for connection in &mut new_connections {
            self.command.configure_upload_connection(
                connection,
                self.piece_length,
                self.num_pieces,
            );
        }
        let mut context = NewPeerConnectionsContext {
            peer_last_data_time: &mut self.peer_last_data_time,
            pex_enabled_peers: self.pex_enabled_peers,
            allowed_fast_sent_peers: &mut self.command.allowed_fast_sent_peers,
            suggest_sent_counts: &mut self.command.suggest_sent_counts,
            peer_tracker: &mut self.peer_tracker,
            choking_algo: &mut self.command.choking_algo,
        };
        BtDownloadCommand::append_new_connections(
            self.active_connections,
            new_connections,
            max_peers,
            is_private,
            &mut context,
            &self.command.peer_storage,
            caretaker_id,
        )
    }

    async fn announce_available_pieces(&mut self) {
        let bitfield = self
            .completed_bitfield
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for connection in self.active_connections.iter_mut() {
            connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
            connection.set_upload_progress(std::sync::Arc::clone(&self.command.progress));
            if let Err(error) = connection.send_bitfield(bitfield.clone()).await {
                tracing::debug!(%error, "Failed to announce BT upload availability");
            }
        }
    }
}
