use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::download::execute::types::DiscoveredPeer;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::piece::selector::BtPieceSelector;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;
use tracing::info;

use super::super::BtStopTimeoutState;
use super::PieceDownloadSession;
use crate::engine::bittorrent::download::execute::types::{EndgameState, PeerKey};

impl<'a> PieceDownloadSession<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn new(
        command: &'a mut BtDownloadCommand,
        initial_peers: Vec<DiscoveredPeer>,
        network_info_hash: [u8; 20],
        swarm: &'a mut PeerSwarm,
        upload_counter: Arc<std::sync::atomic::AtomicU64>,
        meta: &'a mut aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        web_seed_manager: Option<
            Arc<crate::engine::bittorrent::download::web_seed::WebSeedManager>,
        >,
        last_pex_send: &'a mut Instant,
        verified_piece_indices: &[usize],
    ) -> Result<Self> {
        swarm.remove_dead().await;
        let discovered_peer_count = initial_peers.len();
        let initial_peers = initial_peers
            .into_iter()
            .filter(|peer| {
                peer.address
                    .to_socket_addr()
                    .ok()
                    .is_none_or(|endpoint| !swarm.has_endpoint(endpoint))
            })
            .collect::<Vec<_>>();
        tracing::debug!(
            discovered_peer_count,
            already_connected_peer_count =
                discovered_peer_count.saturating_sub(initial_peers.len()),
            retained_actor_count = swarm.len(),
            "Reconciled initial peer candidates with retained swarm"
        );
        // Single-file torrents are written with a positioned + cached writer:
        // BT downloads pieces out of order (RarestFirst etc.), so writes must
        // target the piece offset — the old sequential `write()` appended
        // pieces in arrival order and silently corrupted the file whenever
        // pieces did not arrive in index order. The write-back cache also
        // coalesces adjacent pieces before flushing (C++ WrDiskCache usage).
        // Multi-file torrents go through the coalesced per-file writer below.
        let cache_size_bytes = command.group.recover().options().disk_cache_size_bytes();
        let mut upload_write_cache = None;
        let raw_writer: Box<dyn SeekableDiskWriter> = if command.multi_file_layout.is_none() {
            let cached_writer = CachedDiskWriter::new_with_mmap_bytes(
                &command.output_path,
                Some(total_size),
                cache_size_bytes,
                false,
            );
            upload_write_cache = cached_writer.cache_handle();
            Box::new(cached_writer)
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
                crate::engine::bittorrent::piece::PieceManager::new_v2(
                    num_pieces,
                    piece_length,
                    total_size,
                    files,
                )
            } else {
                crate::engine::bittorrent::piece::PieceManager::new_hybrid_owned(
                    num_pieces,
                    piece_length,
                    total_size,
                    sha1_hashes,
                    files,
                )
            }
        } else {
            crate::engine::bittorrent::piece::PieceManager::new_owned(
                num_pieces,
                piece_length,
                total_size,
                sha1_hashes,
            )
        };
        drop(piece_layers);
        drop(v2_files);

        let mut piece_picker = crate::engine::bittorrent::piece::PiecePicker::new(num_pieces);
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
        piece_picker
            .set_strategy(crate::engine::bittorrent::piece::PieceSelectionStrategy::RarestFirst);

        let allowed_pieces = {
            let group = command.group.recover();
            group.get_download_context().and_then(|context| {
                crate::engine::bittorrent::piece::selector::allowed_piece_indices(
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
                Some(context) => {
                    crate::engine::bittorrent::piece::selector::prioritized_piece_indices(
                        &rules,
                        context.get_file_entries(),
                        piece_length as u64,
                    )
                    .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
                }
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

        let upload_provider: Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider> =
            Arc::new(
                crate::engine::bittorrent::piece::downloader::FileBackedPieceProvider::with_shared_bitfield(
                    command.output_path.clone(),
                    piece_length,
                    num_pieces,
                    command.multi_file_layout.clone(),
                    Arc::clone(&completed_bitfield),
                )
                .with_write_cache(upload_write_cache),
            );
        let mut active_connections = command
            .connect_to_peers(
                &initial_peers,
                &network_info_hash,
                meta.info_hash_v2,
                num_pieces,
                piece_length,
                total_size,
            )
            .await?;
        let peer_tracker = crate::engine::bittorrent::piece::PeerBitfieldTracker::new(num_pieces);
        let upload_config = crate::engine::bittorrent::peer::upload_session::BtSeedingConfig {
            max_upload_bytes_per_sec: command.group.recover().options().max_upload_limit,
            global_limiter: command.global_limiter.clone(),
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };
        let payload_config = Arc::new(
            crate::engine::bittorrent::peer::message_handler::PeerActorPayloadConfig {
                network_info_hash,
                local_metadata: Arc::clone(&command.local_metadata),
                piece_length,
                num_pieces,
                total_length: total_size,
                upload_config: upload_config.clone(),
                upload_limiter: command.torrent_upload_limiter.clone(),
                auto_unchoke: command.choking_algo.is_none(),
                upload_counter: Arc::clone(&upload_counter),
                upload_progress: Arc::clone(&command.progress),
                provider: Arc::clone(&upload_provider),
            },
        );
        let activated_metadata_peers = swarm.activate_payload_actors(payload_config).await;
        if activated_metadata_peers > 0 {
            info!(
                peers = activated_metadata_peers,
                "Reused magnet metadata peer actors for payload transfer"
            );
        }
        for actor in swarm.iter().filter(|actor| !actor.dead) {
            command.track_peer_for_upload_choking(&actor.stats);
        }
        for connection in active_connections.iter_mut() {
            connection.configure_upload_with_auto_unchoke(
                &upload_config,
                command.torrent_upload_limiter.clone(),
                num_pieces,
                piece_length,
                command.choking_algo.is_none(),
            );
            connection.set_upload_counter(Arc::clone(&upload_counter));
        }
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

        let initial_endpoints = active_connections
            .iter()
            .filter_map(BtPeerConn::remote_endpoint)
            .collect::<Vec<_>>();
        let upload_progress = std::sync::Arc::clone(&command.progress);
        let last_uploaded = command.total_uploaded;
        let provider = std::sync::Arc::clone(&upload_provider);
        for mut connection in active_connections {
            connection.set_upload_progress(std::sync::Arc::clone(&upload_progress));
            command.track_peer_for_upload_choking(&connection.stats);
            let peer_dht_engine = connection
                .remote_endpoint()
                .and_then(|endpoint| command.dht_engines.for_peer(endpoint));
            if let Err(_connection) = swarm.spawn_peer(
                connection,
                peer_dht_engine,
                std::sync::Arc::clone(&provider),
            ) {
                swarm.shutdown_all().await;
                let mut peer_storage = command
                    .peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for endpoint in &initial_endpoints {
                    peer_storage
                        .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
                }
                return Err(Aria2Error::DownloadFailed(
                    "Failed to transfer an initial BitTorrent peer to its swarm actor".into(),
                ));
            }
        }
        command
            .drain_incoming_peers_to_swarm(
                swarm,
                crate::engine::bittorrent::download::execute::incoming::PeerActorAdmissionContext {
                    network_info_hash,
                    piece_length,
                    num_pieces,
                    total_size,
                    provider: std::sync::Arc::clone(&upload_provider),
                    upload_counter: std::sync::Arc::clone(&upload_counter),
                },
            )
            .await;
        {
            let group = command.group.recover();
            swarm.attach_peer_snapshot_store(group.bt_peer_snapshot_store());
        }
        let live_peer_count = swarm.iter().filter(|actor| !actor.dead).count();
        command.bt_runtime.set_connections(live_peer_count);
        command
            .group
            .recover()
            .set_bt_connection_count(live_peer_count);
        if live_peer_count > 0 {
            let group = command.group.recover();
            super::super::sync_peer_snapshots_with_swarm(&group, swarm);
        }

        Ok(Self {
            command,
            swarm,
            meta,
            network_info_hash,
            piece_length,
            total_size,
            num_pieces,
            web_seed_manager,
            pending_pex_peers: Vec::new(),
            pending_tracker_peers: Vec::new(),
            last_pex_send,
            writer,
            start_time,
            last_upload_speed_update: Instant::now(),
            last_uploaded,
            upload_counter,
            last_progress_save,
            piece_selector,
            has_v1_piece_hashes,
            piece_manager,
            piece_picker,
            completed_bitfield,
            upload_provider,
            peer_tracker,
            endgame_state,
            request_timeout,
            peer_last_data_time,
            last_snub_check,
            stop_timeout,
        })
    }
}
