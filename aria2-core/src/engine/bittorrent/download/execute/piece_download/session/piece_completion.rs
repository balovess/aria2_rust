use std::sync::Arc;
use std::time::Instant;

use crate::engine::bittorrent::download::execute::types::PeerKey;
use crate::engine::bittorrent::peer::message_handler::types::PieceDownloadResult;
use crate::engine::work_commit::{
    WorkCommitOutcome, WorkCommitter, WorkValidation, commit_work_result,
};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadResultCode;
use crate::util::rwlock_ext::RwLockRecover;

use super::piece_storage::in_flight_snapshot;
use super::{PieceDownloadSession, PieceLoopAction};

struct PieceWorkCommitter<'a, 'command> {
    session: &'a mut PieceDownloadSession<'command>,
    piece_index: u32,
    progress_index: usize,
}

#[async_trait::async_trait]
impl WorkCommitter for PieceWorkCommitter<'_, '_> {
    type Output = Vec<u8>;

    async fn validate(&mut self, data: &mut Self::Output) -> Result<WorkValidation> {
        let expected_hash = self
            .session
            .piece_manager
            .expected_piece_verification(self.piece_index);
        let (verified, verified_data) =
            super::super::super::hash_verification::verify_piece_hash_async(
                expected_hash,
                std::mem::take(data),
            )
            .await?;
        if !verified {
            return Ok(WorkValidation::Rejected);
        }
        *data = verified_data;
        Ok(WorkValidation::Accepted)
    }

    async fn write(&mut self, data: Self::Output) -> Result<u64> {
        let data_length = data.len() as u64;
        let piece_bytes = bytes::Bytes::from(data);
        if self.session.command.multi_file_layout.is_some() {
            let max_open_files = self
                .session
                .command
                .group
                .recover()
                .options()
                .bt_max_open_files;
            self.session
                .command
                .write_multi_file_piece_and_track(self.piece_index, &piece_bytes, max_open_files)
                .await?;
        } else {
            self.session
                .writer
                .write_bytes_at(
                    self.piece_index as u64 * self.session.piece_length as u64,
                    piece_bytes,
                )
                .await?;
        }

        let committed_bytes =
            if self.session.meta.info.meta_version == Some(2) && self.session.has_v1_piece_hashes {
                self.session
                    .command
                    .multi_file_layout
                    .as_ref()
                    .map(|layout| layout.content_bytes_in_piece(self.piece_index))
                    .unwrap_or(data_length)
            } else {
                data_length
            };

        Ok(committed_bytes)
    }

    async fn persist(&mut self, _committed_bytes: u64) -> Result<()> {
        self.session
            .command
            .sync_checkpoint_payload(&mut self.session.writer)
            .await
    }

    async fn checkpoint(&mut self, committed_bytes: u64) -> Result<()> {
        self.session
            .piece_manager
            .mark_piece_complete(self.piece_index);
        self.session.piece_picker.mark_completed(self.piece_index);
        super::super::super::checkpoint::mark_piece_completed(
            &self.session.completed_bitfield,
            self.piece_index,
        );
        self.session.command.completed_bytes = self
            .session
            .command
            .completed_bytes
            .saturating_add(committed_bytes);
        self.session.in_flight_pieces.remove(&self.piece_index);
        self.session
            .command
            .group
            .recover()
            .update_bt_bitfield_piece(self.piece_index, self.session.num_pieces);
        self.session
            .command
            .persist_checkpoint_after_piece(
                &mut self.session.writer,
                &self.session.completed_bitfield,
                committed_bytes,
                &in_flight_snapshot(&self.session.in_flight_pieces),
                true,
            )
            .await?;
        self.session.swarm.set_wanted_pieces(Arc::from(
            self.session.piece_picker.missing_pieces_bitfield(),
        ));
        self.session.swarm.broadcast_have(self.piece_index).await;
        self.session.command.maybe_save_progress(
            self.session.meta,
            &self.session.completed_bitfield,
            self.session.piece_length,
            self.session.total_size,
            self.session.num_pieces,
            self.session.start_time,
            &mut self.session.last_progress_save,
            self.progress_index,
        );
        Ok(())
    }
}

impl PieceDownloadSession<'_> {
    async fn commit_piece_work(&mut self, piece_index: u32, data: Vec<u8>) -> Result<bool> {
        let mut committer = PieceWorkCommitter {
            session: self,
            piece_index,
            progress_index: piece_index as usize,
        };
        Ok(matches!(
            commit_work_result(&mut committer, data).await?,
            WorkCommitOutcome::Committed
        ))
    }

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
        if !self.commit_piece_work(piece_index, web_seed_data).await? {
            tracing::warn!(piece_index, "WebSeed piece failed hash verification");
            self.piece_picker.mark_reserved(piece_index, false);
            return Ok(false);
        }
        Ok(true)
    }

    async fn try_web_seed_fallback(
        &mut self,
        piece_index: usize,
        piece_data_length: u32,
    ) -> Result<bool> {
        let Some(web_seed_manager) = self.web_seed_manager.as_deref() else {
            return Ok(false);
        };

        tracing::info!(
            "[BT] Piece {} failed from peers, trying web seeds...",
            piece_index
        );
        let connection_guard = crate::request::request_group::ActiveConnectionGuard::new(
            Arc::clone(&self.command.group),
        );
        connection_guard.set(1);
        let result = web_seed_manager
            .request_piece_with_length_and_activity(
                piece_index as u32,
                piece_data_length as u64,
                Some(self.command.progress.as_ref()),
            )
            .await
            .map_err(|error| error.to_string());
        drop(connection_guard);

        if let Ok(data) = &result {
            if !data.is_empty() {
                self.command.progress.record_network_activity();
            }
            tracing::info!(
                "[BT] Piece {} downloaded from web seed ({} bytes)",
                piece_index,
                data.len()
            );
        }
        self.complete_web_seed_piece(piece_index as u32, result)
            .await
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
                if self
                    .commit_piece_work(next_piece_idx as u32, piece_data)
                    .await?
                {
                    tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                    piece_ok = true;
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
            piece_ok = self
                .try_web_seed_fallback(next_piece_idx, actual_piece_len)
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
