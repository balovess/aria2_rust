use std::time::Instant;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadResultCode, HaltReason};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::{debug, info, warn};

use super::super::peer_events::NewPeerConnectionsContext;
use super::{PieceDownloadSession, PieceLoopAction};

impl PieceDownloadSession<'_> {
    pub(super) async fn run(mut self) -> Result<()> {
        loop {
            self.command.drain_incoming_peers(
                self.active_connections,
                self.piece_length,
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
                    let bitfield =
                        super::super::super::snapshot_completed_bitfield(&self.completed_bitfield);
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
            // Peers are accumulated during block reads (in
            // BtMessageHandler::wait_for_piece_block) and stashed on
            // BtPeerConn::pending_pex_peers. Here we drain them and add
            // to our known-peers list.
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
                        &self.meta.network_info_hash(),
                        self.num_pieces,
                        self.active_connections,
                        self.piece_length,
                        self.total_size,
                    )
                    .await;
                let connected = self.append_new_connections(new_connections);
                if connected > 0 {
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
                            &self.meta.network_info_hash(),
                            self.num_pieces,
                            self.active_connections,
                            self.piece_length,
                            self.total_size,
                        )
                        .await;
                    let connected = self.append_new_connections(new_connections);
                    if connected > 0 {
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
                        &self.meta.network_info_hash(),
                        self.num_pieces,
                        self.active_connections,
                        self.piece_length,
                        self.total_size,
                    )
                    .await;
                let connected = self.append_new_connections(new_connections);
                if connected > 0 {
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
            if self.active_connections.is_empty() && self.web_seed_manager.is_none() {
                debug!("[BT] No peers available, waiting for peer discovery...");
                let deadline = self.command.next_peer_event_deadline(
                    self.active_connections,
                    self.stop_timeout.deadline(),
                );
                let event = self
                    .command
                    .wait_for_peer_event(self.active_connections, deadline)
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
                        self.total_size,
                    );
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
                    tracing::debug!("[BT] No piece available, waiting...");
                    let deadline = self.command.next_peer_event_deadline(
                        self.active_connections,
                        self.stop_timeout.deadline(),
                    );
                    let event = self
                        .command
                        .wait_for_peer_event(self.active_connections, deadline)
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
                            self.total_size,
                        );
                    }
                    BtDownloadCommand::send_due_keepalives(self.active_connections).await;
                    continue;
                }
            };

            if matches!(
                self.download_piece(next_piece_idx).await?,
                PieceLoopAction::RefreshProgress
            ) {
                {
                    self.command
                        .progress
                        .set_completed_length(self.command.completed_bytes);

                    let elapsed = self.last_speed_update.elapsed();
                    if elapsed.as_millis() >= 500 {
                        let delta = self.command.completed_bytes - self.last_completed;
                        let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                        self.command.progress.set_download_speed(speed);
                        self.command.progress.set_upload_speed(0);
                        self.last_speed_update = Instant::now();
                        self.last_completed = self.command.completed_bytes;
                    }
                }
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
}

impl PieceDownloadSession<'_> {
    fn append_new_connections(&mut self, new_connections: Vec<BtPeerConn>) -> usize {
        let max_peers = self.command.group.recover().options().bt_max_peers;
        let caretaker_id = self.command.group.recover().gid().value();
        let is_private = self.command.is_private;
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
}
