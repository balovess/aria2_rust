use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::message_handler::types::{
    BLOCK_SIZE, PieceDownloadResult, PieceRequestPlan, ReceivedPieceBlock,
};
use crate::engine::bittorrent::peer::message_handler::{
    download_piece_blocks_batch, download_piece_blocks_endgame,
};
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFileInFlightPiece;
use crate::filesystem::disk_writer::SeekableDiskWriter;
use crate::request::request_group::DownloadResultCode;
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::{PieceDownloadSession, PieceLoopAction};
use crate::engine::bittorrent::download::execute::types::PeerKey;

async fn persist_received_block(
    writer: &mut Box<dyn SeekableDiskWriter>,
    layout: Option<&crate::engine::bittorrent::torrent::file_layout::MultiFileLayout>,
    piece_lengths: &HashMap<u32, u32>,
    in_flight: &mut HashMap<u32, ControlFileInFlightPiece>,
    dirty_multi_file_indices: &mut HashSet<usize>,
    max_open_files: usize,
    block: ReceivedPieceBlock,
) -> Result<()> {
    let Some(&piece_length) = piece_lengths.get(&block.piece_index) else {
        return Ok(());
    };
    let block_count = piece_length.div_ceil(BLOCK_SIZE);
    let expected_offset = block.block_index.saturating_mul(BLOCK_SIZE);
    let expected_length = (piece_length.saturating_sub(expected_offset)).min(BLOCK_SIZE);
    if block.block_index >= block_count
        || block.offset != expected_offset
        || block.data.len() != expected_length as usize
    {
        return Err(Aria2Error::Network(format!(
            "Invalid received block layout for piece {} block {}",
            block.piece_index, block.block_index
        )));
    }

    if let Some(layout) = layout {
        let touched_files =
            crate::engine::bittorrent::piece::downloader::write_piece_block_to_multi_files(
                layout,
                block.piece_index,
                block.offset,
                &block.data,
                max_open_files,
            )
            .await?;
        dirty_multi_file_indices.extend(touched_files);
    } else {
        let global_offset =
            u64::from(block.piece_index) * u64::from(piece_length) + u64::from(block.offset);
        writer.write_bytes_at(global_offset, block.data).await?;
    }

    let bitfield_len = (block_count as usize).div_ceil(8);
    let record = in_flight
        .entry(block.piece_index)
        .or_insert_with(|| ControlFileInFlightPiece {
            index: block.piece_index,
            length: piece_length,
            bitfield: vec![0; bitfield_len],
        });
    if record.length != piece_length || record.bitfield.len() != bitfield_len {
        *record = ControlFileInFlightPiece {
            index: block.piece_index,
            length: piece_length,
            bitfield: vec![0; bitfield_len],
        };
    }
    record.bitfield[block.block_index as usize / 8] |= 1 << (7 - block.block_index % 8);
    Ok(())
}

pub(super) fn in_flight_snapshot(
    in_flight: &HashMap<u32, ControlFileInFlightPiece>,
) -> Vec<ControlFileInFlightPiece> {
    let mut pieces = in_flight.values().cloned().collect::<Vec<_>>();
    pieces.sort_unstable_by_key(|piece| piece.index);
    pieces
}

impl PieceDownloadSession<'_> {
    pub(super) async fn complete_web_seed_piece(
        &mut self,
        piece_index: u32,
        result: std::result::Result<Vec<u8>, String>,
    ) -> Result<bool> {
        let web_seed_data = match result {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(piece_index, %error, "WebSeed piece request failed");
                self.piece_picker.mark_reserved(piece_index, false);
                return Ok(false);
            }
        };
        let expected_hash = self.piece_manager.expected_piece_verification(piece_index);
        let (verified, web_seed_data) =
            super::super::super::hash_verification::verify_piece_hash_async(
                expected_hash,
                web_seed_data,
            )
            .await?;
        if !verified {
            tracing::warn!(piece_index, "WebSeed piece failed hash verification");
            self.piece_picker.mark_reserved(piece_index, false);
            return Ok(false);
        }

        let web_seed_data_length = web_seed_data.len() as u64;
        let web_seed_bytes = bytes::Bytes::from(web_seed_data);
        if self.command.multi_file_layout.is_some() {
            let max_open_files = self.command.group.recover().options().bt_max_open_files;
            self.command
                .write_multi_file_piece_and_track(piece_index, &web_seed_bytes, max_open_files)
                .await?;
        } else {
            self.writer
                .write_bytes_at(
                    piece_index as u64 * self.piece_length as u64,
                    web_seed_bytes,
                )
                .await?;
        }

        let accounted_bytes = if self.meta.info.meta_version == Some(2) && self.has_v1_piece_hashes
        {
            self.command
                .multi_file_layout
                .as_ref()
                .map(|layout| layout.content_bytes_in_piece(piece_index))
                .unwrap_or(web_seed_data_length)
        } else {
            web_seed_data_length
        };
        self.piece_manager.mark_piece_complete(piece_index);
        self.piece_picker.mark_completed(piece_index);
        super::super::super::checkpoint::mark_piece_completed(
            &self.completed_bitfield,
            piece_index,
        );
        self.swarm
            .set_wanted_pieces(Arc::from(self.piece_picker.missing_pieces_bitfield()));
        self.command.completed_bytes = self.command.completed_bytes.saturating_add(accounted_bytes);
        self.in_flight_pieces.remove(&piece_index);
        self.command
            .group
            .recover()
            .update_bt_bitfield_piece(piece_index, self.num_pieces);
        self.command
            .persist_checkpoint_after_piece(
                &mut self.writer,
                &self.completed_bitfield,
                accounted_bytes,
                &in_flight_snapshot(&self.in_flight_pieces),
            )
            .await?;
        self.swarm.broadcast_have(piece_index).await;
        self.command.maybe_save_progress(
            self.meta,
            &self.completed_bitfield,
            self.piece_length,
            self.total_size,
            self.num_pieces,
            self.start_time,
            &mut self.last_progress_save,
            piece_index as usize,
        );
        Ok(true)
    }

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

    async fn load_resumed_blocks(
        &mut self,
        piece_index: u32,
        piece_length: u32,
    ) -> Result<Vec<Option<bytes::Bytes>>> {
        let block_count = piece_length.div_ceil(BLOCK_SIZE) as usize;
        let mut blocks = vec![None; block_count];
        let Some(mut record) = self.in_flight_pieces.get(&piece_index).cloned() else {
            return Ok(blocks);
        };
        let expected_bitfield_len = block_count.div_ceil(8);
        if self.piece_picker.is_completed(piece_index)
            || record.length != piece_length
            || record.bitfield.len() != expected_bitfield_len
        {
            self.in_flight_pieces.remove(&piece_index);
            return Ok(blocks);
        }

        for (block_index, block) in blocks.iter_mut().enumerate() {
            let mask = 1 << (7 - block_index % 8);
            if record.bitfield[block_index / 8] & mask == 0 {
                continue;
            }
            let offset = block_index as u32 * BLOCK_SIZE;
            let length = (piece_length - offset).min(BLOCK_SIZE) as usize;
            let bytes = if let Some(layout) = self.command.multi_file_layout.as_ref() {
                crate::engine::bittorrent::piece::downloader::read_piece_range_from_files(
                    layout,
                    piece_index,
                    offset,
                    length as u32,
                )
                .await
            } else {
                let mut data = vec![0; length];
                let mut read = 0;
                while read < data.len() {
                    match self
                        .writer
                        .read_at(
                            u64::from(piece_index) * u64::from(piece_length)
                                + u64::from(offset)
                                + read as u64,
                            &mut data[read..],
                        )
                        .await
                    {
                        Ok(0) | Err(_) => break,
                        Ok(bytes_read) => read += bytes_read,
                    }
                }
                (read == data.len()).then_some(data)
            };
            if let Some(bytes) = bytes {
                *block = Some(bytes::Bytes::from(bytes));
            } else {
                record.bitfield[block_index / 8] &= !mask;
            }
        }

        if record.bitfield.iter().all(|byte| *byte == 0) {
            self.in_flight_pieces.remove(&piece_index);
        } else {
            self.in_flight_pieces.insert(piece_index, record);
        }
        Ok(blocks)
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
                            persist_received_block(
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
                persist_received_block(
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
                            persist_received_block(
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
                persist_received_block(
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

    async fn process_piece_download_result(
        &mut self,
        next_piece_idx: usize,
        actual_piece_len: u32,
        download_result: Result<PieceDownloadResult>,
        peer_actor_ids: Vec<crate::engine::bittorrent::peer::connection::PeerActorId>,
    ) -> Result<PieceLoopAction> {
        let mut piece_ok = false;
        match download_result {
            Ok(piece_result) => {
                let piece_data = piece_result.data;
                let piece_data_len = piece_data.len();

                // Apply receive-time attribution before failed peers are removed.
                for (peer_download, actor_id) in piece_result.peer_bytes.iter().zip(peer_actor_ids)
                {
                    let Some(actor) = self.swarm.actor(actor_id) else {
                        tracing::debug!(
                            actor_id = actor_id.0,
                            peer = %peer_download.peer,
                            "Discarding byte accounting for a retired peer actor"
                        );
                        continue;
                    };
                    if actor.endpoint != peer_download.peer {
                        tracing::debug!(actor_id = actor_id.0, peer = %peer_download.peer, "Discarding byte accounting for mismatched peer actor");
                        continue;
                    }
                    self.peer_last_data_time
                        .insert(PeerKey::new(actor.endpoint), Instant::now());
                }

                for actor in self.swarm.iter_mut() {
                    if piece_result.failed_peers.contains(&actor.endpoint) {
                        actor.dead = true;
                    }
                }
                if self.remove_dead_swarm_peers().await {
                    self.apply_upload_choke_round();
                }
                self.command.update_tracker_peer_state(self.swarm.len());
                {
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
                }

                tracing::info!(
                    "[BT] All blocks received for piece {}, verifying...",
                    next_piece_idx
                );
                let expected_hash = self
                    .piece_manager
                    .expected_piece_verification(next_piece_idx as u32);
                let (piece_verified, piece_data) =
                    super::super::super::hash_verification::verify_piece_hash_async(
                        expected_hash,
                        piece_data,
                    )
                    .await?;
                if piece_verified {
                    tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                    self.piece_manager
                        .mark_piece_complete(next_piece_idx as u32);
                    self.piece_picker.mark_completed(next_piece_idx as u32);

                    let piece_bytes = bytes::Bytes::from(piece_data);
                    if self.command.multi_file_layout.is_some() {
                        let max_open_files =
                            self.command.group.recover().options().bt_max_open_files;
                        self.command
                            .write_multi_file_piece_and_track(
                                next_piece_idx as u32,
                                &piece_bytes,
                                max_open_files,
                            )
                            .await?;
                    } else {
                        self.writer
                            .write_bytes_at(
                                next_piece_idx as u64 * self.piece_length as u64,
                                piece_bytes,
                            )
                            .await?;
                    }

                    super::super::super::checkpoint::mark_piece_completed(
                        &self.completed_bitfield,
                        next_piece_idx as u32,
                    );

                    let accounted_piece_len =
                        if self.meta.info.meta_version == Some(2) && self.has_v1_piece_hashes {
                            self.command
                                .multi_file_layout
                                .as_ref()
                                .map(|layout| layout.content_bytes_in_piece(next_piece_idx as u32))
                                .unwrap_or(piece_data_len as u64)
                        } else {
                            piece_data_len as u64
                        };
                    self.command.completed_bytes += accounted_piece_len;
                    self.in_flight_pieces.remove(&(next_piece_idx as u32));

                    self.command
                        .group
                        .recover()
                        .update_bt_bitfield_piece(next_piece_idx as u32, self.num_pieces);
                    self.command
                        .persist_checkpoint_after_piece(
                            &mut self.writer,
                            &self.completed_bitfield,
                            accounted_piece_len,
                            &in_flight_snapshot(&self.in_flight_pieces),
                        )
                        .await?;

                    self.swarm
                        .set_wanted_pieces(Arc::from(self.piece_picker.missing_pieces_bitfield()));
                    self.swarm.broadcast_have(next_piece_idx as u32).await;
                    piece_ok = true;

                    // P1 integration: periodically save download progress
                    self.command.maybe_save_progress(
                        self.meta,
                        &self.completed_bitfield,
                        self.piece_length,
                        self.total_size,
                        self.num_pieces,
                        self.start_time,
                        &mut self.last_progress_save,
                        next_piece_idx,
                    );
                } else {
                    self.in_flight_pieces.remove(&(next_piece_idx as u32));
                    tracing::warn!(
                        "[BT] SHA1 mismatch on piece {}, retrying...",
                        next_piece_idx
                    );
                    tracing::warn!(
                        "[BT] Piece {} hash verification FAILED - potential bad peer detected",
                        next_piece_idx
                    );
                    let mut peer_bytes = piece_result.peer_bytes.iter();
                    let unique_peer = peer_bytes
                        .next()
                        .filter(|first| peer_bytes.all(|peer| peer.peer == first.peer));
                    let bad_peer = unique_peer.map(|peer_download| peer_download.peer);
                    if let Some(peer) = bad_peer {
                        let peer_ip = peer.ip().to_string();
                        self.command.reject_peer_temporarily(&peer_ip);
                        tracing::warn!(
                            peer = %peer,
                            piece = next_piece_idx,
                            "Rejected and removed peer after a piece hash mismatch"
                        );
                        for actor in self.swarm.iter_mut().filter(|actor| actor.endpoint == peer) {
                            actor.dead = true;
                        }
                        if self.remove_dead_swarm_peers().await {
                            self.apply_upload_choke_round();
                        }
                        self.command.update_tracker_peer_state(self.swarm.len());
                        super::super::sync_peer_snapshots_with_swarm(
                            &self.command.group.recover(),
                            self.swarm,
                        );
                    } else {
                        tracing::debug!(
                            piece = next_piece_idx,
                            "Piece used multiple or unknown peers; no peer was rejected"
                        );
                    }
                }
            }
            Err(Aria2Error::Network(error)) => {
                tracing::warn!(
                    "[BT] Piece {} did not complete after its network attempts: {}",
                    next_piece_idx,
                    error,
                );
            }
            Err(error) => return Err(error),
        }

        if !piece_ok {
            // Try Web Seeds as fallback (BEP 19)
            let accounted_piece_bytes =
                if self.meta.info.meta_version == Some(2) && self.has_v1_piece_hashes {
                    self.command
                        .multi_file_layout
                        .as_ref()
                        .map(|layout| layout.content_bytes_in_piece(next_piece_idx as u32))
                        .unwrap_or(actual_piece_len as u64)
                } else {
                    actual_piece_len as u64
                };
            piece_ok = super::super::super::web_seed::try_web_seed_fallback(
                self.command,
                self.web_seed_manager.as_deref(),
                next_piece_idx,
                actual_piece_len,
                accounted_piece_bytes,
                &mut self.piece_manager,
                &mut self.piece_picker,
                &self.completed_bitfield,
                self.num_pieces,
                &mut self.writer,
                self.piece_length,
            )
            .await?;

            if !piece_ok {
                let source_count =
                    self.swarm
                        .iter()
                        .filter(|actor| {
                            !actor.dead
                                && (actor.seeder
                                    || actor.bitfield.get(next_piece_idx / 8).is_some_and(|byte| {
                                        byte & (0x80 >> (next_piece_idx % 8)) != 0
                                    }))
                        })
                        .count();
                let failure_message = if source_count == 0 {
                    format!(
                        "Piece {} has no source among {} connected peers",
                        next_piece_idx,
                        self.swarm.len()
                    )
                } else {
                    format!(
                        "Piece {} was advertised by {} connected peer(s), but no peer completed the transfer",
                        next_piece_idx, source_count
                    )
                };
                self.command
                    .group
                    .recover()
                    .set_last_error(DownloadResultCode::NetworkProblem, &failure_message);
                tracing::warn!(
                    "[BT] {} (peers and web seeds); waiting for discovery",
                    failure_message
                );
                // Advertised availability does not prove that a peer can or will
                // transfer the piece. Keep the task alive so tracker, DHT, PEX,
                // incoming peers, and later unchoke events can make progress.
                return Ok(PieceLoopAction::Retry);
            }
        }
        Ok(PieceLoopAction::RefreshProgress)
    }
}

impl BtDownloadCommand {
    /// Periodically save download progress to .aria2 file (P1 integration).
    /// Called after a piece is successfully verified and written.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn maybe_save_progress(
        &self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        start_time: Instant,
        last_progress_save: &mut Instant,
        next_piece_idx: usize,
    ) {
        if let Some(ref mgr) = self.progress_manager
            && last_progress_save.elapsed() >= self.progress_save_interval
        {
            let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(bitfield);
            let progress = super::super::progress_snapshot(
                meta.network_info_hash(),
                &bitfield,
                piece_length,
                total_size,
                num_pieces,
                crate::engine::bittorrent::persistence::progress_info_file::DownloadStats {
                    downloaded_bytes: self.completed_bytes,
                    uploaded_bytes: self.total_uploaded,
                    upload_speed: 0.0,
                    download_speed: 0.0,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
            );

            match mgr.save_progress(&meta.network_info_hash(), &progress) {
                Ok(()) => {
                    *last_progress_save = Instant::now();
                    tracing::debug!(
                        pieces_completed = next_piece_idx + 1,
                        total_pieces = num_pieces,
                        "BT progress saved successfully"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "Failed to save BT progress"),
            }
        }
    }
}
