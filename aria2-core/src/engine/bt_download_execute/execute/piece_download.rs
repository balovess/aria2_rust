use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{
    BLOCK_SIZE, BtDownloadCommand, MAX_RETRIES, PEER_CONNECTION_DELAY_MS,
};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::engine::bt_progress_info_file::{BtProgress, DownloadStats as ProgressDownloadStats};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::super::types::{EndgameState, PeerKey};

fn unique_source_peer(
    source_peers: impl IntoIterator<Item = std::net::SocketAddr>,
) -> Option<std::net::SocketAddr> {
    let mut unique = source_peers.into_iter();
    let first = unique.next()?;
    unique.all(|peer| peer == first).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::unique_source_peer;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn unique_source_peer_accepts_one_source() {
        assert_eq!(unique_source_peer([peer(6881)]), Some(peer(6881)));
    }

    #[test]
    fn unique_source_peer_accepts_repeated_blocks_from_one_source() {
        assert_eq!(
            unique_source_peer([peer(6881), peer(6881), peer(6881)]),
            Some(peer(6881))
        );
    }

    #[test]
    fn unique_source_peer_rejects_mixed_sources() {
        assert_eq!(unique_source_peer([peer(6881), peer(6882)]), None);
    }

    #[test]
    fn unique_source_peer_rejects_unknown_source_set() {
        assert_eq!(unique_source_peer(std::iter::empty()), None);
    }
}

impl BtDownloadCommand {
    // Parameters are individually meaningful; grouping into a struct would
    // reduce clarity for this inner download loop.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn remove_failed_peers(
        active_connections: &mut Vec<BtPeerConn>,
        failed_peers: &[std::net::SocketAddr],
        choking_algo: Option<&mut crate::engine::choking_algorithm::ChokingAlgorithm>,
        pex_enabled_peers: &mut std::collections::HashSet<PeerKey>,
        peer_last_data_time: &mut HashMap<PeerKey, Instant>,
        allowed_fast_sent_peers: &mut HashMap<PeerKey, std::collections::HashSet<u32>>,
        suggest_sent_counts: &mut HashMap<PeerKey, usize>,
        endgame_state: &mut EndgameState,
        peer_tracker: &mut aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
    ) {
        if failed_peers.is_empty() {
            return;
        }
        let failed: HashSet<_> = failed_peers.iter().copied().collect();
        let removed_indices: Vec<_> = active_connections
            .iter()
            .enumerate()
            .filter_map(|(index, conn)| {
                let address = format!("{}:{}", conn.ip_addr, conn.port).parse().ok()?;
                failed.contains(&address).then_some(index)
            })
            .collect();
        if removed_indices.is_empty() {
            return;
        }
        for &index in removed_indices.iter().rev() {
            if let Some(conn) = active_connections.get(index) {
                peer_tracker.remove_peer(&BtPeerInteraction::peer_tracker_key(conn));
            }
        }
        for &index in removed_indices.iter().rev() {
            active_connections[index].release_session_resource();
        }
        if let Some(algo) = choking_algo {
            algo.remove_peers(removed_indices.as_slice());
        }
        let removed_keys: Vec<_> = removed_indices
            .iter()
            .filter_map(|&index| active_connections.get(index))
            .filter_map(|conn| {
                format!("{}:{}", conn.ip_addr, conn.port)
                    .parse()
                    .ok()
                    .map(crate::engine::bt_download_execute::types::PeerKey::new)
            })
            .collect();
        endgame_state.remove_peers(&removed_keys);
        let mut removed = Vec::new();
        active_connections.retain_mut(|conn| {
            let address = match format!("{}:{}", conn.ip_addr, conn.port).parse() {
                Ok(address) => address,
                Err(_) => return true,
            };
            if failed.contains(&address) {
                removed.push(address);
                false
            } else {
                true
            }
        });
        if removed.is_empty() {
            return;
        }
        pex_enabled_peers.clear();
        peer_last_data_time.clear();
        allowed_fast_sent_peers.clear();
        suggest_sent_counts.clear();
        for index in 0..active_connections.len() {
            if let Some(peer_key) = active_connections
                .get(index)
                .and_then(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port))
            {
                suggest_sent_counts.insert(peer_key, 0);
                pex_enabled_peers.insert(peer_key);
            }
            if let Some(peer_key) = active_connections
                .get(index)
                .and_then(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port))
            {
                peer_last_data_time.insert(peer_key, Instant::now());
            }
            if let Some(peer_key) = active_connections
                .get(index)
                .and_then(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port))
            {
                allowed_fast_sent_peers.insert(peer_key, HashSet::new());
            }
        }
    }

    pub(super) async fn download_pieces_loop(
        &mut self,
        active_connections: &mut Vec<BtPeerConn>,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        web_seed_manager: Option<&crate::engine::bt_web_seed::WebSeedManager>,
        pex_enabled_peers: &mut HashSet<PeerKey>,
        last_pex_send: &mut Instant,
        pex_send_interval_secs: u64,
    ) -> Result<()> {
        // Single-file torrents are written with a positioned + cached writer:
        // BT downloads pieces out of order (RarestFirst etc.), so writes must
        // target the piece offset — the old sequential `write()` appended
        // pieces in arrival order and silently corrupted the file whenever
        // pieces did not arrive in index order. The write-back cache also
        // coalesces adjacent pieces before flushing (C++ WrDiskCache usage).
        // Multi-file torrents go through the coalesced per-file writer below.
        let cache_mb: Option<usize> = Some(16);
        let raw_writer: Box<dyn SeekableDiskWriter> = if self.multi_file_layout.is_none() {
            Box::new(CachedDiskWriter::new(
                &self.output_path,
                Some(total_size),
                cache_mb,
            ))
        } else {
            Box::new(
                crate::filesystem::positioned_disk_writer::PositionedDiskWriter::new(
                    &self.output_path,
                    Some(total_size),
                ),
            )
        };
        let rate_limit = {
            let g = self.group.recover();
            g.options().max_download_limit
        };
        // Global (process-wide) limiter: when present and enabled, the writer
        // acquires tokens after the per-download limiter so all concurrent
        // downloads share a single bandwidth ceiling.
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let mut writer: Box<dyn SeekableDiskWriter> = if rate_limit.is_some() || global_limited {
            let per_limiter = rate_limit
                .filter(|&r| r > 0)
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)));
            let limiter = per_limiter.unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
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
        let mut peer_last_data_time: HashMap<PeerKey, Instant> = HashMap::new();
        let mut last_snub_check = Instant::now();

        // Initialize last-data-time tracking for all active peers
        for conn in active_connections.iter() {
            if let Some(key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                peer_last_data_time.insert(key, Instant::now());
            }
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
            self.check_and_mark_snubbed_peers(
                &mut last_snub_check,
                &peer_last_data_time,
                active_connections,
            );

            // PEX Integration: Periodic PEX message sending (BEP 11)
            super::pex::send_periodic_pex(
                self,
                active_connections,
                pex_enabled_peers,
                last_pex_send,
                pex_send_interval_secs,
            )
            .await;

            // PEX Integration: Drain inbound PEX peers from all connections.
            // Peers are accumulated during block reads (in
            // BtMessageHandler::wait_for_piece_block) and stashed on
            // BtPeerConn::pending_pex_peers. Here we drain them and add
            // to our known-peers list.
            let mut all_new_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
                Vec::new();
            for conn in active_connections.iter_mut() {
                let peers = conn.drain_pex_peers();
                if !peers.is_empty() {
                    for peer in &peers {
                        self.add_pex_peer(peer.clone());
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
                    .connect_to_pex_discovered_peers(
                        &all_new_pex_peers,
                        &meta.info_hash.bytes,
                        num_pieces,
                        active_connections,
                        piece_length,
                        total_size,
                    )
                    .await;
                let previous_len = active_connections.len();
                active_connections.extend(new_connections);
                let connected = active_connections.len() - previous_len;
                for index in previous_len..active_connections.len() {
                    let conn = &active_connections[index];
                    let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) else {
                        continue;
                    };
                    peer_last_data_time.insert(peer_key, Instant::now());
                    self.allowed_fast_sent_peers.entry(peer_key).or_default();
                    self.suggest_sent_counts.entry(peer_key).or_insert(0);
                    let bitfield = conn
                        .session_resource
                        .as_ref()
                        .map_or(&[][..], |resource| resource.bitfield());
                    peer_tracker
                        .update_peer_bitfield(&BtPeerInteraction::peer_tracker_key(conn), bitfield);
                    if !self.is_private {
                        pex_enabled_peers.insert(peer_key);
                    }
                }
                if let Some(algo) = self.choking_algo.as_mut() {
                    for conn in &active_connections[previous_len..] {
                        algo.add_peer(conn.stats.clone());
                    }
                }
                if connected > 0 {
                    info!("[PEX] Successfully connected to {} new peers", connected);
                }
            }

            // Periodic tracker re-announce for peer discovery.
            // C++ aria2 uses TrackerWatcherCommand which checks
            // BtAnnounce::isAnnounceReady() on each iteration.
            // If we have too few active connections, try to discover more peers.
            if active_connections.len() < 5 {
                let new_peers = self
                    .periodic_tracker_announce(
                        &meta.info_hash.bytes,
                        self.completed_bytes,
                        total_size.saturating_sub(self.completed_bytes),
                        self.total_uploaded,
                    )
                    .await;
                if !new_peers.is_empty() {
                    info!(
                        "[BT] Periodic tracker announce found {} new peers",
                        new_peers.len()
                    );
                    // Connect to newly discovered peers
                    let new_connections = self
                        .connect_to_pex_discovered_peers(
                            &new_peers,
                            &meta.info_hash.bytes,
                            num_pieces,
                            active_connections,
                            piece_length,
                            total_size,
                        )
                        .await;
                    let previous_len = active_connections.len();
                    active_connections.extend(new_connections);
                    let connected = active_connections.len() - previous_len;
                    for index in previous_len..active_connections.len() {
                        let conn = &active_connections[index];
                        if let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                            peer_last_data_time.insert(peer_key, Instant::now());
                            self.allowed_fast_sent_peers
                                .insert(peer_key, HashSet::new());
                            self.suggest_sent_counts.insert(peer_key, 0);
                        }
                        let key = BtPeerInteraction::peer_tracker_key(conn);
                        let bitfield = conn
                            .session_resource
                            .as_ref()
                            .map_or(&[][..], |resource| resource.bitfield());
                        peer_tracker.update_peer_bitfield(&key, bitfield);
                        if !self.is_private {
                            if let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                                pex_enabled_peers.insert(peer_key);
                            }
                        }
                    }
                    if let Some(algo) = self.choking_algo.as_mut() {
                        for conn in &active_connections[previous_len..] {
                            algo.add_peer(conn.stats.clone());
                        }
                    }
                    if connected > 0 {
                        info!("[BT] Connected to {} tracker-discovered peers", connected);
                    }
                }
            }

            let remaining = piece_picker.remaining_count();
            let selection = piece_selector.select_next_piece(&mut piece_picker, remaining);

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
                BtMessageHandler::download_piece_blocks_endgame_with_sources(
                    active_connections,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                    &mut endgame_state,
                    self.dht_engine.clone(),
                )
                .await
            } else {
                BtMessageHandler::download_piece_blocks_with_sources(
                    active_connections,
                    next_piece_idx as u32,
                    actual_piece_len,
                    num_blocks,
                    self.dht_engine.clone(),
                )
                .await
            };

            match download_result {
                Ok(piece_result) => {
                    Self::remove_failed_peers(
                        active_connections,
                        &piece_result.failed_peers,
                        self.choking_algo.as_mut(),
                        pex_enabled_peers,
                        &mut peer_last_data_time,
                        &mut self.allowed_fast_sent_peers,
                        &mut self.suggest_sent_counts,
                        // Remove failed peers from endgame duplicate-request tracking.
                        &mut endgame_state,
                        &mut peer_tracker,
                    );
                    let piece_data = piece_result.data;
                    let piece_data_len = piece_data.len();

                    // G1: Update last-data-time for all active peers on successful receive
                    for peer_addr in piece_result.source_peers.iter().copied() {
                        if let Some(peer_key) = active_connections.iter().find_map(|conn| {
                            (format!("{}:{}", conn.ip_addr, conn.port)
                                .parse::<std::net::SocketAddr>()
                                .ok()
                                == Some(peer_addr))
                            .then(|| PeerKey::from_peer(&conn.ip_addr, conn.port))
                            .flatten()
                        }) {
                            peer_last_data_time.insert(peer_key, Instant::now());
                        }
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
                            writer
                                .write_at(next_piece_idx as u64 * piece_length as u64, &piece_data)
                                .await?;
                        }

                        self.completed_bytes += piece_data_len as u64;

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
                        if let Some(peer_addr) = unique_source_peer(piece_result.source_peers) {
                            let peer_ip = peer_addr.ip().to_string();
                            self.reject_peer_temporarily(&peer_ip);
                            tracing::warn!(
                                peer = %peer_addr,
                                piece = next_piece_idx,
                                "Rejected peer after a piece hash mismatch"
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
                piece_ok = super::web_seed::try_web_seed_fallback(
                    self,
                    web_seed_manager,
                    next_piece_idx,
                    &mut piece_manager,
                    &mut piece_picker,
                    &mut writer,
                    piece_length,
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
        writer
            .flush()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to flush BT output: {error}")))?;
        writer
            .close()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to close BT output: {error}")))?;
        tracing::info!("[BT] Writer flushed and closed OK");
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
