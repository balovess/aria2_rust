use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{BtDownloadCommand, BLOCK_SIZE, MAX_RETRIES, PEER_CONNECTION_DELAY_MS};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::engine::bt_progress_info_file::{BtProgress, DownloadStats as ProgressDownloadStats};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::super::types::EndgameState;

impl BtDownloadCommand {
    // Parameters are individually meaningful; grouping into a struct would
    // reduce clarity for this inner download loop.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn download_pieces_loop(
        &mut self,
        active_connections: &mut [BtPeerConn],
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        web_seed_manager: Option<&crate::engine::bt_web_seed::WebSeedManager>,
        pex_enabled_peers: &mut HashSet<usize>,
        last_pex_send: &mut Instant,
        pex_send_interval_secs: u64,
    ) -> Result<()> {
        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let rate_limit = {
            let g = self.group.recover();
            g.options().max_download_limit
        };
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => Box::new(ThrottledWriter::new(
                raw_writer,
                RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
            )),
            _ => Box::new(raw_writer),
        };
        let start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        // P1 integration: progress save time tracking
        let mut last_progress_save = Instant::now();

        let piece_selector = BtPieceSelector::new(num_pieces);

        let mut piece_manager = aria2_protocol::bittorrent::piece::manager::PieceManager::new(
            num_pieces,
            piece_length,
            total_size,
            meta.info.pieces.clone(),
        );

        let mut piece_picker =
            aria2_protocol::bittorrent::piece::picker::PiecePicker::new(num_pieces);
        piece_picker.set_strategy(
            aria2_protocol::bittorrent::piece::picker::PieceSelectionStrategy::Sequential,
        );

        // G2: Set piece priority mode from config option (--bt-prioritize-piece)
        let prioritize_piece_mode = {
            let g = self.group.recover();
            g.options().bt_prioritize_piece.clone()
        };
        match prioritize_piece_mode.as_str() {
            "head" => {
                piece_picker.set_priority_mode(
                    aria2_protocol::bittorrent::piece::picker::PiecePriorityMode::SequentialHead,
                );
                info!("[BT] Piece priority mode: SequentialHead (from start)");
            }
            "tail" => {
                piece_picker.set_priority_mode(
                    aria2_protocol::bittorrent::piece::picker::PiecePriorityMode::SequentialTail,
                );
                info!("[BT] Piece priority mode: SequentialTail (from end)");
            }
            _ => {
                piece_picker.set_priority_mode(
                    aria2_protocol::bittorrent::piece::picker::PiecePriorityMode::RarestFirst,
                );
                info!("[BT] Piece priority mode: RarestFirst (default)");
            }
        }

        let mut peer_tracker =
            aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker::new(num_pieces);
        BtPeerInteraction::initialize_peer_tracking(
            active_connections,
            num_pieces,
            &mut peer_tracker,
        );

        piece_selector.initialize_frequencies(&mut piece_picker, &peer_tracker);

        tracing::info!(
            "[BT] Piece selection strategy: {:?}, {} pieces total, {} peers tracked",
            piece_picker.priority_mode(),
            num_pieces,
            peer_tracker.peer_count()
        );

        // Phase 14 - B1: Initialize endgame state for this download session
        let mut endgame_state = EndgameState::new();

        // G1: Snub detection state - track last data received time per peer index
        let mut peer_last_data_time: HashMap<usize, Instant> = HashMap::new();
        let mut last_snub_check = Instant::now();

        // Initialize last-data-time tracking for all active peers
        for (idx, _conn) in active_connections.iter().enumerate() {
            peer_last_data_time.insert(idx, Instant::now());
        }

        loop {
            if BtPieceSelector::is_complete(&piece_picker) {
                if endgame_state.is_endgame_active() {
                    endgame_state.exit_endgame();
                }
                break;
            }

            // Phase 14 - B1: Check if we should enter endgame mode
            let endgame_candidates = piece_picker.endgame_candidates();
            if !endgame_candidates.is_empty() && !endgame_state.is_endgame_active() {
                endgame_state.enter_endgame();
                info!(
                    "[BT] Endgame mode activated: {}/{} pieces remaining",
                    endgame_candidates.len(),
                    num_pieces
                );
            } else if endgame_candidates.is_empty() && endgame_state.is_endgame_active() {
                endgame_state.exit_endgame();
            }

            // G1: Periodic snub detection via extracted helper
            self.check_and_mark_snubbed_peers(&mut last_snub_check, &peer_last_data_time);

            // PEX Integration: Periodic PEX message sending (BEP 11)
            super::pex::send_periodic_pex(
                self,
                active_connections,
                pex_enabled_peers,
                last_pex_send,
                pex_send_interval_secs,
            );

            let remaining = piece_picker.remaining_count();
            let selection = piece_selector.select_next_piece(&mut piece_picker, remaining as usize);

            let next_piece_idx = match selection.piece_index {
                Some(idx) => idx,
                None => {
                    tracing::debug!("[BT] No piece available, waiting...");
                    tokio::time::sleep(Duration::from_millis(PEER_CONNECTION_DELAY_MS)).await;
                    continue;
                }
            };

            tracing::info!("[BT] Downloading piece {}...", next_piece_idx);

            let actual_piece_len =
                piece_selector.calculate_piece_length(next_piece_idx, piece_length, total_size);

            let num_blocks = BtPieceSelector::calculate_num_blocks(actual_piece_len, BLOCK_SIZE);
            tracing::debug!(
                "[BT] Piece {} has {} blocks (size: {} bytes)",
                next_piece_idx,
                num_blocks,
                actual_piece_len
            );
            let mut piece_ok = false;

            // Phase 14 - B1: Use endgame-aware download when in endgame mode
            let download_result = if endgame_state.is_endgame_active() {
                info!(
                    "[BT] Endgame: downloading piece {} with duplicate requests ({} peers available)",
                    next_piece_idx,
                    active_connections.len()
                );
                BtMessageHandler::download_piece_blocks_endgame(
                    active_connections,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                    &mut endgame_state,
                )
                .await
            } else {
                BtMessageHandler::download_piece_blocks(
                    active_connections,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                )
                .await
            };

            match download_result {
                Ok(piece_data) => {
                    self.completed_bytes += piece_data.len() as u64;

                    // G1: Update last-data-time for all active peers on successful receive
                    for idx in 0..active_connections.len() {
                        peer_last_data_time.insert(idx, Instant::now());
                    }

                    tracing::info!(
                        "[BT] All blocks received for piece {}, verifying...",
                        next_piece_idx
                    );
                    if piece_manager.verify_piece_hash(next_piece_idx as u32, &piece_data) {
                        tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);

                        if let Some(ref layout) = self.multi_file_layout {
                            let piece_bytes = bytes::Bytes::from(piece_data);
                            write_piece_to_multi_files_coalesced(
                                layout,
                                next_piece_idx as u32,
                                &piece_bytes,
                                layout.piece_length(),
                            )
                            .await?;
                        } else {
                            writer.write(&piece_data).await?;
                        }

                        // Sync bitfield to RequestGroup for session persistence
                        {
                            let bitfield = piece_picker.export_bitfield();
                            let g = self.group.recover();
                            g.set_bt_bitfield(Some(bitfield));
                        }

                        BtPeerInteraction::broadcast_have(
                            active_connections,
                            next_piece_idx as u32,
                        )
                        .await;
                        piece_ok = true;

                        // PEX Integration: Trigger PEX send on piece completion
                        if !self.pex_known_peers.is_empty() && self.should_send_pex() {
                            let dummy_remote =
                                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                                    "0.0.0.0", 0,
                                );
                            if let Some(_pex_data) = self.maybe_send_pex(&dummy_remote) {
                                debug!(
                                    "[PEX] PEX message ready after piece {} completion",
                                    next_piece_idx
                                );
                            }
                        }

                        // P1 integration: periodically save download progress
                        self.maybe_save_progress(
                            meta,
                            piece_length,
                            total_size,
                            num_pieces,
                            start_time,
                            &mut last_progress_save,
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
                piece_ok = super::web_seed::try_web_seed_fallback(
                    self,
                    web_seed_manager,
                    next_piece_idx,
                    &mut piece_manager,
                    &mut piece_picker,
                    &mut writer,
                )
                .await?;

                if !piece_ok {
                    tracing::error!(
                        "[BT] Piece {} failed after {} retries (peers and web seeds)",
                        next_piece_idx,
                        MAX_RETRIES
                    );
                    return Err(Aria2Error::Fatal(FatalError::Config(format!(
                        "Piece {} download failed after {} retries",
                        next_piece_idx, MAX_RETRIES
                    ))));
                }
            }

            {
                self.progress.set_completed_length(self.completed_bytes);

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    self.progress.set_download_speed(speed);
                    self.progress.set_upload_speed(0);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        tracing::info!("[BT] Finalizing writer...");
        writer.finalize().await.ok();
        tracing::info!("[BT] Writer finalized OK");
        info!(
            "BT download done: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );

        Ok(())
    }

    /// Periodically save download progress to .aria2 file (P1 integration).
    /// Called after a piece is successfully verified and written.
    #[allow(clippy::too_many_arguments)]
    fn maybe_save_progress(
        &self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
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
            let progress = BtProgress {
                info_hash: meta.info_hash.bytes,
                bitfield: vec![],
                peers: vec![],
                stats: ProgressDownloadStats {
                    downloaded_bytes: self.completed_bytes,
                    uploaded_bytes: self.total_uploaded,
                    upload_speed: 0.0,
                    download_speed: 0.0,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
                piece_length,
                total_size,
                num_pieces,
                upload_length: self.total_uploaded,
                in_flight_pieces: vec![],
                is_torrent: true,
                save_time: std::time::SystemTime::now(),
                version: 1,
            };

            if let Err(e) = mgr.save_progress(&meta.info_hash.bytes, &progress) {
                warn!(error = %e, "Failed to save BT progress");
            } else {
                debug!(
                    pieces_completed = next_piece_idx + 1,
                    total_pieces = num_pieces,
                    "BT progress saved successfully"
                );
            }
            *last_progress_save = Instant::now();
        }
    }
}
