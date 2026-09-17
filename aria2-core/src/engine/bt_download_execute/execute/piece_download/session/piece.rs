use std::time::Instant;

use crate::engine::bt_download_command::{BLOCK_SIZE, BtDownloadCommand};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::DownloadResultCode;
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::{PieceDownloadSession, PieceLoopAction};
use crate::engine::bt_download_execute::types::PeerKey;

impl PieceDownloadSession<'_> {
    pub(super) async fn download_piece(
        &mut self,
        next_piece_idx: usize,
    ) -> Result<PieceLoopAction> {
        tracing::info!("[BT] Downloading piece {}...", next_piece_idx);

        let actual_piece_len =
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
            };

        let num_blocks = BtPieceSelector::calculate_num_blocks(actual_piece_len, BLOCK_SIZE);
        tracing::debug!(
            "[BT] Piece {} has {} blocks (size: {} bytes)",
            next_piece_idx,
            num_blocks,
            actual_piece_len
        );
        let mut piece_ok = false;
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
                    "[BT] Endgame: downloading piece {} with duplicate requests ({} peers available)",
                    next_piece_idx,
                    self.active_connections.len()
                );
                BtMessageHandler::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts(
                                self.active_connections,
                                next_piece_idx as u32,
                                actual_piece_len,
                                num_blocks,
                                &mut self.endgame_state,
                                self.command.dht_engine.clone(),
                                Some(self.command.progress.as_ref()),
                                self.request_timeout,
                                max_attempts,
                            )
                            .await
            } else {
                BtMessageHandler::download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts(
                                self.active_connections,
                                next_piece_idx as u32,
                                actual_piece_len,
                                num_blocks,
                                self.command.dht_engine.clone(),
                                Some(self.command.progress.as_ref()),
                                self.request_timeout,
                                max_attempts,
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
                        super::super::super::snapshot_completed_bitfield(&self.completed_bitfield);
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

        match download_result {
            Ok(piece_result) => {
                let piece_data = piece_result.data;
                let piece_data_len = piece_data.len();

                // Consume peer indices before failed connections are removed and the Vec compacts.
                for peer_download in &piece_result.peer_bytes {
                    let Some(conn) = self.active_connections.get(peer_download.peer_index) else {
                        tracing::debug!(
                            peer_index = peer_download.peer_index,
                            peer = %peer_download.peer,
                            "Discarding peer byte accounting for stale connection index"
                        );
                        continue;
                    };
                    let Some(address) = format!("{}:{}", conn.ip_addr, conn.port)
                        .parse::<std::net::SocketAddr>()
                        .ok()
                    else {
                        continue;
                    };
                    if address != peer_download.peer {
                        tracing::debug!(
                            peer_index = peer_download.peer_index,
                            expected = %peer_download.peer,
                            actual = %address,
                            "Discarding peer byte accounting for mismatched connection"
                        );
                        continue;
                    }
                    let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) else {
                        continue;
                    };
                    self.active_connections[peer_download.peer_index]
                        .stats
                        .on_data_received(peer_download.bytes);
                    self.command
                        .on_data_received_from_peer(peer_download.peer_index, peer_download.bytes);
                    self.peer_last_data_time.insert(peer_key, Instant::now());
                }

                BtDownloadCommand::remove_failed_peers(
                    self.active_connections,
                    &piece_result.failed_peers,
                    self.command.choking_algo.as_mut(),
                    self.pex_enabled_peers,
                    &mut self.peer_last_data_time,
                    &mut self.command.allowed_fast_sent_peers,
                    &mut self.command.suggest_sent_counts,
                    &mut self.endgame_state,
                    &mut self.peer_tracker,
                    &self.command.peer_storage,
                );
                self.command
                    .update_tracker_peer_state(self.active_connections.len());
                {
                    let group = self.command.group.recover();
                    super::super::sync_peer_snapshots(&group, self.active_connections);
                }

                tracing::info!(
                    "[BT] All blocks received for piece {}, verifying...",
                    next_piece_idx
                );
                let expected_hash = self
                    .piece_manager
                    .expected_piece_verification(next_piece_idx as u32);
                let (piece_verified, piece_data) =
                    super::super::super::verify_piece_hash_async(expected_hash, piece_data).await?;
                if piece_verified {
                    tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                    self.piece_manager
                        .mark_piece_complete(next_piece_idx as u32);
                    self.piece_picker.mark_completed(next_piece_idx as u32);

                    let piece_bytes = bytes::Bytes::from(piece_data);
                    if let Some(ref layout) = self.command.multi_file_layout {
                        let max_open_files =
                            self.command.group.recover().options().bt_max_open_files;
                        crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced_with_limit(
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

                    BtPeerInteraction::broadcast_have(
                        self.active_connections,
                        next_piece_idx as u32,
                    )
                    .await;
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
                        BtDownloadCommand::remove_failed_peers(
                            self.active_connections,
                            &[peer],
                            self.command.choking_algo.as_mut(),
                            self.pex_enabled_peers,
                            &mut self.peer_last_data_time,
                            &mut self.command.allowed_fast_sent_peers,
                            &mut self.command.suggest_sent_counts,
                            &mut self.endgame_state,
                            &mut self.peer_tracker,
                            &self.command.peer_storage,
                        );
                        self.command
                            .update_tracker_peer_state(self.active_connections.len());
                        super::super::sync_peer_snapshots(
                            &self.command.group.recover(),
                            self.active_connections,
                        );
                    } else {
                        tracing::debug!(
                            piece = next_piece_idx,
                            "Piece used multiple or unknown peers; no peer was rejected"
                        );
                    }
                }
            }
            Err(_) => {
                tracing::warn!(
                    "[BT] Incomplete piece {}, needed {} blocks",
                    next_piece_idx,
                    num_blocks
                );
            }
        }

        if !piece_ok {
            // Try Web Seeds as fallback (BEP 19)
            piece_ok = super::super::super::web_seed::try_web_seed_fallback(
                self.command,
                self.web_seed_manager,
                next_piece_idx,
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
                    super::super::count_piece_sources(self.active_connections, next_piece_idx);
                let failure_message = if source_count == 0 {
                    format!(
                        "Piece {} has no source among {} connected peers",
                        next_piece_idx,
                        self.active_connections.len()
                    )
                } else {
                    format!(
                        "Piece {} failed after {} retries; {} connected peer(s) advertise it",
                        next_piece_idx, max_attempts, source_count
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
                if source_count == 0 {
                    // A peer snapshot is not a swarm-wide availability proof. The
                    // missing piece may become available after the next tracker,
                    // DHT, PEX, or incoming-peer event, so keep the download
                    // resumable and let bt-stop-timeout provide the final bound.
                    return Ok(PieceLoopAction::Retry);
                }
                return Err(Aria2Error::Fatal(FatalError::Config(failure_message)));
            }
        }
        Ok(PieceLoopAction::RefreshProgress)
    }
}

impl BtDownloadCommand {
    /// Periodically save download progress to .aria2 file (P1 integration).
    /// Called after a piece is successfully verified and written.
    #[allow(clippy::too_many_arguments)]
    fn maybe_save_progress(
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
            let bitfield = super::super::super::snapshot_completed_bitfield(bitfield);
            let progress = super::super::progress_snapshot(
                meta.network_info_hash(),
                &bitfield,
                piece_length,
                total_size,
                num_pieces,
                crate::engine::bt_progress_info_file::DownloadStats {
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
