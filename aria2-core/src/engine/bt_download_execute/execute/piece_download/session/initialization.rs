use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::super::BtStopTimeoutState;
use super::PieceDownloadSession;
use crate::engine::bt_download_execute::types::{EndgameState, PeerKey};

impl<'a> PieceDownloadSession<'a> {
    pub(super) fn new(
        command: &'a mut BtDownloadCommand,
        active_connections: &'a mut Vec<BtPeerConn>,
        meta: &'a mut aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        web_seed_manager: Option<&'a crate::engine::bt_web_seed::WebSeedManager>,
        pex_enabled_peers: &'a mut HashSet<PeerKey>,
        last_pex_send: &'a mut Instant,
        pex_send_interval_secs: u64,
        verified_piece_indices: &[usize],
    ) -> Result<Self> {
        // Single-file torrents are written with a positioned + cached writer:
        // BT downloads pieces out of order (RarestFirst etc.), so writes must
        // target the piece offset — the old sequential `write()` appended
        // pieces in arrival order and silently corrupted the file whenever
        // pieces did not arrive in index order. The write-back cache also
        // coalesces adjacent pieces before flushing (C++ WrDiskCache usage).
        // Multi-file torrents go through the coalesced per-file writer below.
        let cache_size_bytes = command.group.recover().options().disk_cache_size_bytes();
        let raw_writer: Box<dyn SeekableDiskWriter> = if command.multi_file_layout.is_none() {
            Box::new(CachedDiskWriter::new_with_mmap_bytes(
                &command.output_path,
                Some(total_size),
                cache_size_bytes,
                false,
            ))
        } else {
            Box::new(
                crate::filesystem::positioned_disk_writer::PositionedDiskWriter::new(
                    &command.output_path,
                    Some(total_size),
                ),
            )
        };
        let rate_limit = {
            let g = command.group.recover();
            g.options().max_download_limit
        };
        // Global (process-wide) limiter: when present and enabled, the writer
        // acquires tokens after the per-download limiter so all concurrent
        // downloads share a single bandwidth ceiling.
        let global_limited = command
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let writer: Box<dyn SeekableDiskWriter> = if rate_limit.is_some() || global_limited {
            let per_limiter = rate_limit
                .filter(|&r| r > 0)
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)));
            let limiter = per_limiter.unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = command.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            raw_writer
        };
        let start_time = Instant::now();
        let last_speed_update = Instant::now();
        let last_completed = 0u64;

        // P1 integration: progress save time tracking
        let last_progress_save = Instant::now();

        let piece_selector = BtPieceSelector::new(num_pieces);

        // PieceManager owns the hashes needed during the download loop. The
        // parsed piece-layer map is only an input to its construction, so
        // release that second representation before entering the long-lived
        // scheduling loop.
        let piece_layers = std::mem::take(&mut meta.piece_layers);
        let sha1_hashes = std::mem::take(&mut meta.info.pieces);
        let has_v1_piece_hashes = !sha1_hashes.is_empty();
        let v2_files = std::mem::take(&mut meta.info.v2_files);
        let mut piece_manager = if meta.info.meta_version == Some(2) {
            let files = v2_files
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|file| {
                    let hashes = file.pieces_root.map_or_else(Vec::new, |root| {
                        piece_layers.get(&root).cloned().unwrap_or_else(|| {
                            if file.length <= piece_length as u64 {
                                vec![root]
                            } else {
                                Vec::new()
                            }
                        })
                    });
                    (file.length, hashes)
                })
                .collect();
            if sha1_hashes.is_empty() {
                crate::engine::bt_piece::PieceManager::new_v2(
                    num_pieces,
                    piece_length,
                    total_size,
                    files,
                )
            } else {
                crate::engine::bt_piece::PieceManager::new_hybrid_owned(
                    num_pieces,
                    piece_length,
                    total_size,
                    sha1_hashes,
                    files,
                )
            }
        } else {
            crate::engine::bt_piece::PieceManager::new_owned(
                num_pieces,
                piece_length,
                total_size,
                sha1_hashes,
            )
        };
        drop(piece_layers);
        drop(v2_files);

        let mut piece_picker = crate::engine::bt_piece::PiecePicker::new(num_pieces);
        if meta.info.meta_version == Some(2) {
            for index in 0..num_pieces {
                if piece_manager.expected_piece_verification(index).is_none() {
                    piece_picker.mark_completed(index);
                    piece_manager.mark_piece_complete(index);
                }
            }
        }
        // aria2_original uses RarestPieceSelector as the base BitTorrent
        // selector. `bt-prioritize-piece` is an additive wrapper around it,
        // not a replacement for the torrent-wide selection strategy.
        piece_picker.set_strategy(crate::engine::bt_piece::PieceSelectionStrategy::RarestFirst);

        let allowed_pieces = {
            let group = command.group.recover();
            group.get_download_context().and_then(|context| {
                crate::engine::bt_piece_selector::allowed_piece_indices(
                    &context,
                    piece_length as u64,
                    num_pieces,
                )
            })
        };
        if let Some(allowed_pieces) = allowed_pieces {
            info!(
                "[BT] Selective file filter enabled: {} of {} pieces selected",
                allowed_pieces.len(),
                num_pieces
            );
            piece_picker.set_allowed_pieces(&allowed_pieces);
        }

        if !command.check_integrity
            && let Some(bitfield) = command
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.bitfield())
        {
            for index in 0..num_pieces as usize {
                if bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
                {
                    piece_picker.mark_completed(index as u32);
                    piece_manager.mark_piece_complete(index as u32);
                }
            }
        }

        let prioritized_pieces = {
            let group = command.group.recover();
            let rules = crate::config::parse_piece_priority(&group.options().bt_prioritize_piece)
                .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?;
            match group.get_download_context() {
                Some(context) => crate::engine::bt_piece_selector::prioritized_piece_indices(
                    &rules,
                    context.get_file_entries(),
                    piece_length as u64,
                )
                .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?,
                None => Vec::new(),
            }
        };
        if !prioritized_pieces.is_empty() {
            info!(
                "[BT] Prioritizing {} file-boundary pieces from bt-prioritize-piece",
                prioritized_pieces.len()
            );
            piece_picker.set_priority_pieces(prioritized_pieces);
        }

        let mut peer_tracker = crate::engine::bt_piece::PeerBitfieldTracker::new(num_pieces);
        BtPeerInteraction::initialize_peer_tracking(
            active_connections,
            num_pieces,
            &mut peer_tracker,
        );

        for &index in verified_piece_indices {
            if index < num_pieces as usize {
                piece_picker.mark_completed(index as u32);
                piece_manager.mark_piece_complete(index as u32);
            }
        }
        let completed_bitfield =
            std::sync::Arc::new(std::sync::RwLock::new(piece_picker.export_bitfield()));
        command
            .group
            .recover()
            .set_bt_bitfield_shared(std::sync::Arc::clone(&completed_bitfield));
        piece_selector.initialize_frequencies(&mut piece_picker, &peer_tracker);

        tracing::info!(
            "[BT] Piece selection strategy: {:?}, {} pieces total, {} peers tracked",
            piece_picker.priority_mode(),
            num_pieces,
            peer_tracker.peer_count()
        );

        // Phase 14 - B1: Initialize endgame state for this download session
        let endgame_state = EndgameState::new();
        let request_timeout = {
            let group = command.group.recover();
            Duration::from_secs(group.options().bt_request_timeout.max(1))
        };

        // G1: Snub detection state - track last data received time per peer index
        let mut peer_last_data_time: HashMap<PeerKey, Instant> = HashMap::new();
        let last_snub_check = Instant::now();
        let stop_timeout = BtStopTimeoutState::new(Instant::now(), command.completed_bytes);

        // Initialize last-data-time tracking for all active peers
        for conn in active_connections.iter() {
            if let Some(key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                peer_last_data_time.insert(key, Instant::now());
            }
        }
        Ok(Self {
            command,
            active_connections,
            meta,
            piece_length,
            total_size,
            num_pieces,
            web_seed_manager,
            pex_enabled_peers,
            last_pex_send,
            pex_send_interval_secs,
            writer,
            start_time,
            last_speed_update,
            last_completed,
            last_progress_save,
            piece_selector,
            has_v1_piece_hashes,
            piece_manager,
            piece_picker,
            completed_bitfield,
            peer_tracker,
            endgame_state,
            request_timeout,
            peer_last_data_time,
            last_snub_check,
            stop_timeout,
        })
    }
}
