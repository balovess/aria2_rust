use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::bittorrent::peer::message_handler::types::{BLOCK_SIZE, PieceRequestPlan};
use crate::engine::bittorrent::peer::message_handler::{
    download_piece_blocks_batch, download_piece_blocks_endgame,
};
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::piece_storage::{in_flight_snapshot, stage_received_block};
use super::{PieceDownloadSession, PieceLoopAction};

impl PieceDownloadSession<'_> {
    pub(super) fn actual_piece_length(&self, next_piece_idx: usize) -> u32 {
        if self.meta.info.meta_version == Some(2) && !self.has_v1_piece_hashes {
            self.command
                .multi_file_layout
                .as_ref()
                .map(|layout| layout.content_bytes_in_piece(next_piece_idx as u32))
                .filter(|&length| length > 0)
                .map(|length| length as u32)
                .unwrap_or_else(|| {
                    self.piece_selector.calculate_piece_length(
                        next_piece_idx,
                        self.piece_length,
                        self.total_size,
                    )
                })
        } else if self.meta.info.meta_version == Some(2) {
            self.command
                .multi_file_layout
                .as_ref()
                .map(|layout| {
                    self.piece_selector.calculate_piece_length(
                        next_piece_idx,
                        self.piece_length,
                        layout.piece_space_size(),
                    )
                })
                .unwrap_or_else(|| {
                    self.piece_selector.calculate_piece_length(
                        next_piece_idx,
                        self.piece_length,
                        self.total_size,
                    )
                })
        } else {
            self.piece_selector.calculate_piece_length(
                next_piece_idx,
                self.piece_length,
                self.total_size,
            )
        }
    }

    pub(super) async fn download_piece(
        &mut self,
        next_piece_idx: usize,
    ) -> Result<PieceLoopAction> {
        tracing::info!("[BT] Downloading piece {}...", next_piece_idx);
        self.swarm
            .set_wanted_pieces(Arc::from(self.piece_picker.missing_pieces_bitfield()));

        let actual_piece_len = self.actual_piece_length(next_piece_idx);

        let num_blocks = BtPieceSelector::calculate_num_blocks(actual_piece_len, BLOCK_SIZE);
        tracing::debug!(
            "[BT] Piece {} has {} blocks (size: {} bytes)",
            next_piece_idx,
            num_blocks,
            actual_piece_len
        );
        let max_attempts = self.command.group.recover().options().max_retries;
        let piece_index = next_piece_idx as u32;
        let resume_blocks = self
            .load_resumed_blocks(piece_index, actual_piece_len)
            .await?;
        let block_tx = tokio::sync::mpsc::channel(64);
        let (block_sender, mut block_receiver) = block_tx;
        let piece_lengths = HashMap::from([(piece_index, actual_piece_len)]);
        let max_open_files = self.command.group.recover().options().bt_max_open_files;
        let request_timeout = self.request_timeout;
        let endgame_active = self.endgame_state.is_endgame_active();
        let swarm_len = self.swarm.len();

        // Phase 14 - B1: Use endgame-aware download when in endgame mode
        // A block read can otherwise wait for the full protocol timeout
        // after pause/remove. Keep the low-level message handler focused
        // on peer I/O and let the owning RequestGroup interrupt the whole
        // piece future through its lifecycle notification.
        let lifecycle_notify = self.command.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        lifecycle_wait.as_mut().enable();
        let dht_notify = self.command.dht_periodic_lookup.completion_notifier();
        let dht_wait = dht_notify.notified();
        tokio::pin!(dht_wait);
        dht_wait.as_mut().enable();
        let download_result = {
            let writer = &mut self.writer;
            let layout = self.command.multi_file_layout.as_ref();
            let in_flight = &mut self.in_flight_pieces;
            let dirty_multi_file_indices = &mut self.command.dirty_multi_file_indices;
            let swarm = &mut self.swarm;
            let choking_algo = &mut self.command.choking_algo;
            let endgame_state = &mut self.endgame_state;
            let piece_download = async {
                if endgame_active {
                    info!(
                        "[BT] Endgame: downloading piece {} with duplicate requests ({} swarm peers available)",
                        next_piece_idx, swarm_len
                    );
                    download_piece_blocks_endgame(
                        swarm,
                        piece_index,
                        actual_piece_len,
                        num_blocks,
                        endgame_state,
                        request_timeout,
                        max_attempts,
                        choking_algo.as_mut(),
                        &resume_blocks,
                        Some(&block_sender),
                    )
                    .await
                } else {
                    let mut batch = download_piece_blocks_batch(
                        swarm,
                        &[PieceRequestPlan {
                            piece_index,
                            piece_length: actual_piece_len,
                            num_blocks,
                            resume_blocks,
                        }],
                        request_timeout,
                        max_attempts,
                        choking_algo.as_mut(),
                        Some(&block_sender),
                    )
                    .await?;
                    let entry = batch
                        .pieces
                        .pop()
                        .expect("single-piece scheduler returns its requested piece");
                    Ok(crate::engine::bittorrent::peer::message_handler::types::ActorAwarePieceDownloadResult {
                        piece: entry.result.map_err(Aria2Error::Network),
                        peer_actor_ids: entry.peer_actor_ids,
                        availability_changed_actor_ids: batch.availability_changed_actor_ids,
                        pex_peers: batch.pex_peers,
                        tracker_peers: batch.tracker_peers,
                    })
                }
            };
            tokio::pin!(piece_download);
            let result = loop {
                tokio::select! {
                    result = &mut piece_download => break Some(result),
                    block = block_receiver.recv() => {
                        if let Some(block) = block {
                            stage_received_block(
                                writer,
                                layout,
                                &piece_lengths,
                                in_flight,
                                dirty_multi_file_indices,
                                max_open_files,
                                block,
                            ).await?;
                        }
                    }
                    _ = &mut lifecycle_wait => break None,
                    _ = &mut dht_wait => {
                        tracing::debug!(piece_index, "DHT lookup completed during piece download; yielding to peer discovery");
                        break None;
                    }
                }
            };
            while let Ok(block) = block_receiver.try_recv() {
                stage_received_block(
                    writer,
                    layout,
                    &piece_lengths,
                    in_flight,
                    dirty_multi_file_indices,
                    max_open_files,
                    block,
                )
                .await?;
            }
            result
        };
        let download_result = match download_result {
            Some(result) => result,
            None => {
                let halt_requested = {
                    let group = self.command.group.recover();
                    group.is_force_halt_requested() || group.is_halt_requested()
                };
                self.command
                    .sync_checkpoint_payload(&mut self.writer)
                    .await
                    .map_err(|error| {
                        Aria2Error::FileIo(format!(
                            "Failed to sync halted BT output before checkpoint: {error}"
                        ))
                    })?;
                if let Some(checkpoint) = self.command.checkpoint.as_mut() {
                    let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(
                        &self.completed_bitfield,
                    );
                    checkpoint
                        .save_with_in_flight_pieces(
                            &bitfield,
                            self.command.completed_bytes,
                            &in_flight_snapshot(&self.in_flight_pieces),
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
                if halt_requested {
                    self.writer.close().await.map_err(|error| {
                        Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                    })?;
                    return Err(Aria2Error::DownloadFailed(
                        "BitTorrent download halted".into(),
                    ));
                }
                return Ok(PieceLoopAction::Retry);
            }
        };
        let (piece_result, peer_actor_ids) = match download_result {
            Ok(actor_result) => {
                self.pending_pex_peers.extend(actor_result.pex_peers);
                self.pending_tracker_peers
                    .extend(actor_result.tracker_peers);
                super::availability::sync_swarm_actor_availability(
                    self.swarm,
                    &actor_result.availability_changed_actor_ids,
                    &mut self.peer_tracker,
                    &mut self.piece_picker,
                    &mut self.peer_last_data_time,
                );
                (actor_result.piece, actor_result.peer_actor_ids)
            }
            Err(error) => (Err(error), Vec::new()),
        };
        self.process_piece_download_result(
            next_piece_idx,
            actual_piece_len,
            piece_result,
            peer_actor_ids,
        )
        .await
    }

    pub(super) async fn download_piece_batch(
        &mut self,
        piece_indices: &[usize],
    ) -> Result<Vec<(usize, PieceLoopAction)>> {
        if piece_indices.len() < 2 {
            let Some(&piece_index) = piece_indices.first() else {
                return Ok(Vec::new());
            };
            return Ok(vec![(piece_index, self.download_piece(piece_index).await?)]);
        }

        self.swarm
            .set_wanted_pieces(Arc::from(self.piece_picker.missing_pieces_bitfield()));
        let mut plans = Vec::with_capacity(piece_indices.len());
        for &piece_index in piece_indices {
            let piece_length = self.actual_piece_length(piece_index);
            let resume_blocks = self
                .load_resumed_blocks(piece_index as u32, piece_length)
                .await?;
            plans.push(PieceRequestPlan {
                piece_index: piece_index as u32,
                piece_length,
                num_blocks: BtPieceSelector::calculate_num_blocks(piece_length, BLOCK_SIZE),
                resume_blocks,
            });
        }
        let piece_lengths = plans
            .iter()
            .map(|plan| (plan.piece_index, plan.piece_length))
            .collect::<HashMap<_, _>>();
        let max_attempts = self.command.group.recover().options().max_retries;
        let max_open_files = self.command.group.recover().options().bt_max_open_files;
        let lifecycle_notify = self.command.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        lifecycle_wait.as_mut().enable();
        let dht_notify = self.command.dht_periodic_lookup.completion_notifier();
        let dht_wait = dht_notify.notified();
        tokio::pin!(dht_wait);
        dht_wait.as_mut().enable();
        let (block_sender, mut block_receiver) = tokio::sync::mpsc::channel(64);
        let request_timeout = self.request_timeout;
        let batch_result = {
            let writer = &mut self.writer;
            let layout = self.command.multi_file_layout.as_ref();
            let in_flight = &mut self.in_flight_pieces;
            let dirty_multi_file_indices = &mut self.command.dirty_multi_file_indices;
            let swarm = &mut self.swarm;
            let choking_algo = &mut self.command.choking_algo;
            let batch_download = download_piece_blocks_batch(
                swarm,
                &plans,
                request_timeout,
                max_attempts,
                choking_algo.as_mut(),
                Some(&block_sender),
            );
            tokio::pin!(batch_download);
            let result = loop {
                tokio::select! {
                    result = &mut batch_download => break Some(result),
                    block = block_receiver.recv() => {
                        if let Some(block) = block {
                            stage_received_block(
                                writer,
                                layout,
                                &piece_lengths,
                                in_flight,
                                dirty_multi_file_indices,
                                max_open_files,
                                block,
                            ).await?;
                        }
                    }
                    _ = &mut lifecycle_wait => break None,
                    _ = &mut dht_wait => {
                        tracing::debug!(pieces = piece_indices.len(), "DHT lookup completed during piece batch; yielding to peer discovery");
                        break None;
                    }
                }
            };
            while let Ok(block) = block_receiver.try_recv() {
                stage_received_block(
                    writer,
                    layout,
                    &piece_lengths,
                    in_flight,
                    dirty_multi_file_indices,
                    max_open_files,
                    block,
                )
                .await?;
            }
            result
        };
        let batch_result = match batch_result {
            Some(result) => result?,
            None => {
                let halt_requested = {
                    let group = self.command.group.recover();
                    group.is_force_halt_requested() || group.is_halt_requested()
                };
                self.command
                    .sync_checkpoint_payload(&mut self.writer)
                    .await
                    .map_err(|error| {
                        Aria2Error::FileIo(format!(
                            "Failed to sync halted BT output before checkpoint: {error}"
                        ))
                    })?;
                if let Some(checkpoint) = self.command.checkpoint.as_mut() {
                    let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(
                        &self.completed_bitfield,
                    );
                    checkpoint
                        .save_with_in_flight_pieces(
                            &bitfield,
                            self.command.completed_bytes,
                            &in_flight_snapshot(&self.in_flight_pieces),
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
                if halt_requested {
                    self.writer.close().await.map_err(|error| {
                        Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                    })?;
                    return Err(Aria2Error::DownloadFailed(
                        "BitTorrent download halted".into(),
                    ));
                }
                return Ok(piece_indices
                    .iter()
                    .copied()
                    .map(|piece_index| (piece_index, PieceLoopAction::Retry))
                    .collect());
            }
        };

        self.pending_pex_peers.extend(batch_result.pex_peers);
        self.pending_tracker_peers
            .extend(batch_result.tracker_peers);
        super::availability::sync_swarm_actor_availability(
            self.swarm,
            &batch_result.availability_changed_actor_ids,
            &mut self.peer_tracker,
            &mut self.piece_picker,
            &mut self.peer_last_data_time,
        );

        let mut actions = Vec::with_capacity(batch_result.pieces.len());
        for entry in batch_result.pieces {
            let piece_index = entry.piece_index as usize;
            let piece_length = self.actual_piece_length(piece_index);
            let result = entry.result.map_err(Aria2Error::Network);
            let action = self
                .process_piece_download_result(
                    piece_index,
                    piece_length,
                    result,
                    entry.peer_actor_ids,
                )
                .await?;
            actions.push((piece_index, action));
        }
        Ok(actions)
    }
}
