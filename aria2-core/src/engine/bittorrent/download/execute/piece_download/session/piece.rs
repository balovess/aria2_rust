use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::message_handler::types::{
    BLOCK_SIZE, PieceDownloadResult, PieceRequestPlan,
};
use crate::engine::bittorrent::peer::message_handler::{
    download_piece_blocks, download_piece_blocks_batch, download_piece_blocks_endgame,
};
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadResultCode;
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::{PieceDownloadSession, PieceLoopAction};
use crate::engine::bittorrent::download::execute::types::PeerKey;

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
        if let Some(ref layout) = self.command.multi_file_layout {
            let max_open_files = self.command.group.recover().options().bt_max_open_files;
            crate::engine::bittorrent::piece::downloader::write_piece_to_multi_files_coalesced_with_limit(
                layout,
                piece_index,
                &web_seed_bytes,
                layout.piece_length(),
                max_open_files,
            )
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
        self.swarm
            .set_wanted_pieces(Arc::from(self.piece_picker.missing_pieces_bitfield()));
        self.command.completed_bytes = self.command.completed_bytes.saturating_add(accounted_bytes);
        self.command
            .group
            .recover()
            .update_bt_bitfield_piece(piece_index, self.num_pieces);
        self.command
            .persist_checkpoint_after_piece(
                &mut self.writer,
                &self.completed_bitfield,
                accounted_bytes,
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

        // Phase 14 - B1: Use endgame-aware download when in endgame mode
        // A block read can otherwise wait for the full protocol timeout
        // after pause/remove. Keep the low-level message handler focused
        // on peer I/O and let the owning RequestGroup interrupt the whole
        // piece future through its lifecycle notification.
        let lifecycle_notify = self.command.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        lifecycle_wait.as_mut().enable();
        let piece_download = async {
            if self.endgame_state.is_endgame_active() {
                info!(
                    "[BT] Endgame: downloading piece {} with duplicate requests ({} swarm peers available)",
                    next_piece_idx,
                    self.swarm.len()
                );
                download_piece_blocks_endgame(
                    self.swarm,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                    &mut self.endgame_state,
                    self.request_timeout,
                    max_attempts,
                    self.command.choking_algo.as_mut(),
                )
                .await
            } else {
                download_piece_blocks(
                    self.swarm,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                    self.request_timeout,
                    max_attempts,
                    self.command.choking_algo.as_mut(),
                )
                .await
            }
        };
        let download_result = tokio::select! {
            result = piece_download => result,
            _ = &mut lifecycle_wait => {
                let halt_requested = {
                    let group = self.command.group.recover();
                    group.is_force_halt_requested() || group.is_halt_requested()
                };
                if !halt_requested {
                    // Save-session and other non-terminal lifecycle
                    // updates share this notifier. Retry the interrupted
                    // piece so its normal completion boundary can consume
                    // the requested checkpoint.
                    return Ok(PieceLoopAction::Retry);
                }

                self.writer.flush().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to flush halted BT output: {error}"))
                })?;
                self.writer.close().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                })?;
                if let Some(checkpoint) = self.command.checkpoint.as_mut() {
                    let bitfield =
                        super::super::super::checkpoint::snapshot_completed_bitfield(
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
                    self.command.group.recover().take_save_control_file_request();
                }
                return Err(Aria2Error::DownloadFailed(
                    "BitTorrent download halted".into(),
                ));
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
        let plans = piece_indices
            .iter()
            .map(|&piece_index| {
                let piece_length = self.actual_piece_length(piece_index);
                PieceRequestPlan {
                    piece_index: piece_index as u32,
                    piece_length,
                    num_blocks: BtPieceSelector::calculate_num_blocks(piece_length, BLOCK_SIZE),
                }
            })
            .collect::<Vec<_>>();
        let max_attempts = self.command.group.recover().options().max_retries;
        let lifecycle_notify = self.command.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        lifecycle_wait.as_mut().enable();
        let batch_download = download_piece_blocks_batch(
            self.swarm,
            &plans,
            self.request_timeout,
            max_attempts,
            self.command.choking_algo.as_mut(),
        );
        let batch_result = tokio::select! {
            result = batch_download => result?,
            _ = &mut lifecycle_wait => {
                let halt_requested = {
                    let group = self.command.group.recover();
                    group.is_force_halt_requested() || group.is_halt_requested()
                };
                if !halt_requested {
                    return Ok(piece_indices
                        .iter()
                        .copied()
                        .map(|piece_index| (piece_index, PieceLoopAction::Retry))
                        .collect());
                }

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
                    self.command.group.recover().take_save_control_file_request();
                }
                return Err(Aria2Error::DownloadFailed(
                    "BitTorrent download halted".into(),
                ));
            }
        };

        self.pending_pex_peers.extend(batch_result.pex_peers);
        self.pending_tracker_peers
            .extend(batch_result.tracker_peers);
        super::availability::sync_swarm_actor_availability(
            self.swarm,
            &batch_result.availability_changed_actor_ids,
            &mut self.peer_tracker,
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
                    if let Some(ref layout) = self.command.multi_file_layout {
                        let max_open_files =
                            self.command.group.recover().options().bt_max_open_files;
                        crate::engine::bittorrent::piece::downloader::write_piece_to_multi_files_coalesced_with_limit(
                                        layout,
                                        next_piece_idx as u32,
                                        &piece_bytes,
                                        layout.piece_length(),
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

                    self.command
                        .group
                        .recover()
                        .update_bt_bitfield_piece(next_piece_idx as u32, self.num_pieces);
                    self.command
                        .persist_checkpoint_after_piece(
                            &mut self.writer,
                            &self.completed_bitfield,
                            accounted_piece_len,
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
