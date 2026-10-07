use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::download::execute::types::PeerKey;
use crate::engine::bittorrent::peer::message_handler::types::PieceDownloadResult;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadResultCode;
use crate::util::rwlock_ext::RwLockRecover;

use super::piece_storage::in_flight_snapshot;
use super::{PieceDownloadSession, PieceLoopAction};

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

    pub(super) async fn process_piece_download_result(
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
