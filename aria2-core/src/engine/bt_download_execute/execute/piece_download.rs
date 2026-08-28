use futures::StreamExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{BLOCK_SIZE, BtDownloadCommand};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_selector::BtPieceSelector;
use crate::engine::bt_progress_info_file::{BtProgress, DownloadStats as ProgressDownloadStats};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{BtPeerSnapshot, DownloadResultCode, HaltReason};
use crate::util::rwlock_ext::RwLockRecover;

use super::super::types::{EndgameState, PeerKey};

fn progress_snapshot(
    info_hash: [u8; 20],
    bitfield: &[u8],
    piece_length: u32,
    total_size: u64,
    num_pieces: u32,
    stats: ProgressDownloadStats,
) -> BtProgress {
    let upload_length = stats.uploaded_bytes;
    BtProgress {
        info_hash,
        bitfield: bitfield.to_vec(),
        peers: vec![],
        stats,
        piece_length,
        total_size,
        num_pieces,
        upload_length,
        in_flight_pieces: vec![],
        is_torrent: true,
        save_time: std::time::SystemTime::now(),
        version: 1,
    }
}

fn sync_peer_snapshots(
    group: &crate::request::request_group::RequestGroup,
    active_connections: &[BtPeerConn],
) {
    let snapshots: Vec<BtPeerSnapshot> = active_connections
        .iter()
        .filter_map(|conn| {
            Some(BtPeerSnapshot {
                peer_id: conn.peer_id.unwrap_or(conn.stats.peer_id),
                addr: conn.remote_endpoint()?,
                is_incoming: false,
                uploaded_bytes: conn.stats.uploaded_bytes,
                downloaded_bytes: conn.stats.downloaded_bytes,
                upload_speed: conn.stats.upload_speed,
                download_speed: conn.stats.download_speed,
                avg_upload_speed: conn.stats.avg_upload_speed,
                avg_download_speed: conn.stats.avg_download_speed,
                am_choking: conn.stats.am_choking,
                peer_choking: conn.stats.peer_choking,
                seeder: Some(conn.seeder),
                connection_duration_secs: conn.stats.connection_duration_secs(),
                last_data_age_secs: conn
                    .stats
                    .last_data_time
                    .map_or(conn.stats.age().as_secs(), |time| time.elapsed().as_secs()),
                is_snubbed: conn.stats.is_snubbed,
                is_banned: conn.stats.is_banned,
            })
        })
        .collect();
    group.set_bt_connection_count(snapshots.len());
    group.set_bt_peer_snapshots(snapshots);
}

struct NewPeerConnectionsContext<'a> {
    peer_last_data_time: &'a mut HashMap<PeerKey, Instant>,
    pex_enabled_peers: &'a mut HashSet<PeerKey>,
    allowed_fast_sent_peers: &'a mut HashMap<PeerKey, HashSet<u32>>,
    suggest_sent_counts: &'a mut HashMap<PeerKey, usize>,
    peer_tracker: &'a mut aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
    choking_algo: &'a mut Option<crate::engine::choking_algorithm::ChokingAlgorithm>,
}

/// Tracks consecutive BitTorrent time without a completed piece.
///
/// The original `BtStopDownloadCommand` observes the download periodically
/// and resets its checkpoint when the measured download speed is positive.
/// The piece loop already owns the authoritative completed-byte counter, so
/// using it here avoids a stale cached speed keeping a stalled task alive.
struct BtStopTimeoutState {
    configured: Option<Duration>,
    last_progress_at: Instant,
    last_completed_bytes: u64,
}

impl BtStopTimeoutState {
    fn new(now: Instant, completed_bytes: u64) -> Self {
        Self {
            configured: None,
            last_progress_at: now,
            last_completed_bytes: completed_bytes,
        }
    }

    fn should_halt(
        &mut self,
        configured_seconds: Option<u64>,
        completed_bytes: u64,
        now: Instant,
    ) -> bool {
        let configured = configured_seconds
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        if configured != self.configured {
            self.configured = configured;
            self.last_progress_at = now;
        }

        if completed_bytes > self.last_completed_bytes {
            self.last_completed_bytes = completed_bytes;
            self.last_progress_at = now;
        }

        configured.is_some_and(|timeout| now.duration_since(self.last_progress_at) >= timeout)
    }

    fn deadline(&self) -> Option<Instant> {
        self.configured
            .map(|timeout| self.last_progress_at + timeout)
    }
}

enum PeerWaitEvent {
    Incoming(crate::engine::bt_peer_listener::IncomingPeer),
    PeerMessage {
        index: usize,
        result: Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>>,
    },
    Wake,
}

impl BtDownloadCommand {
    fn next_peer_event_deadline(
        &self,
        active_connections: &[BtPeerConn],
        stop_timeout_deadline: Option<Instant>,
    ) -> Instant {
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(24 * 60 * 60);

        if self.dht_engine.is_some()
            && let Some(delay) = self
                .dht_periodic_lookup
                .next_lookup_delay(active_connections.len())
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(delay) = self
            .tracker_announcer
            .as_ref()
            .and_then(|announcer| announcer.next_default_announce_delay())
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(stop_timeout_deadline) = stop_timeout_deadline {
            deadline = deadline.min(stop_timeout_deadline);
        }
        for connection in active_connections {
            deadline = deadline.min(connection.keepalive_deadline());
        }
        deadline
    }

    async fn send_due_keepalives(active_connections: &mut [BtPeerConn]) {
        for connection in active_connections {
            if connection.should_send_keepalive()
                && let Err(error) = connection.send_keepalive().await
            {
                tracing::debug!(
                    peer = %format!("{}:{}", connection.ip_addr, connection.port),
                    %error,
                    "Failed to send configured BitTorrent keep-alive"
                );
            }
        }
    }

    /// Wait for a peer/discovery event instead of waking on a fixed short
    /// delay. Network messages are read concurrently from all active peers;
    /// tracker and DHT timers are only used at their protocol deadlines.
    async fn wait_for_peer_event(
        &mut self,
        active_connections: &mut [BtPeerConn],
        deadline: Instant,
    ) -> PeerWaitEvent {
        let completion_notify = self.dht_periodic_lookup.completion_notifier();
        let completion_wait = completion_notify.notified();
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        let mut incoming_receiver = self.incoming_peers.take();
        let deadline_wait = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline_wait);

        let mut peer_reads = active_connections
            .iter_mut()
            .enumerate()
            .map(|(index, connection)| async move { (index, connection.read_message().await) })
            .collect::<futures::stream::FuturesUnordered<_>>();

        let event = tokio::select! {
            incoming = async {
                match incoming_receiver.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending::<Option<crate::engine::bt_peer_listener::IncomingPeer>>().await,
                }
            } => match incoming {
                Some(incoming) => PeerWaitEvent::Incoming(incoming),
                None => {
                    incoming_receiver = None;
                    PeerWaitEvent::Wake
                }
            },
            peer = peer_reads.next(), if !peer_reads.is_empty() => {
                peer.map_or(PeerWaitEvent::Wake, |(index, result)| {
                    PeerWaitEvent::PeerMessage { index, result }
                })
            },
            _ = completion_wait => PeerWaitEvent::Wake,
            _ = lifecycle_wait => PeerWaitEvent::Wake,
            _ = &mut deadline_wait => PeerWaitEvent::Wake,
        };

        drop(peer_reads);
        self.incoming_peers = incoming_receiver;
        event
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_peer_wait_event(
        event: PeerWaitEvent,
        active_connections: &mut Vec<BtPeerConn>,
        peer_tracker: &mut aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
        pex_enabled_peers: &mut HashSet<PeerKey>,
        peer_last_data_time: &mut HashMap<PeerKey, Instant>,
        allowed_fast_sent_peers: &mut HashMap<PeerKey, HashSet<u32>>,
        suggest_sent_counts: &mut HashMap<PeerKey, usize>,
        endgame_state: &mut EndgameState,
        choking_algo: Option<&mut crate::engine::choking_algorithm::ChokingAlgorithm>,
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
    ) -> Option<crate::engine::bt_peer_listener::IncomingPeer> {
        match event {
            PeerWaitEvent::Incoming(incoming) => return Some(incoming),
            PeerWaitEvent::PeerMessage { index, result } => {
                let failed_address = {
                    let connection = active_connections.get_mut(index)?;
                    let peer_key = PeerKey::from_peer(&connection.ip_addr, connection.port);
                    match result {
                        Ok(Some(message)) => {
                            let before = connection
                                .session_resource
                                .as_ref()
                                .map(|resource| resource.bitfield().to_vec());
                            match message {
                                aria2_protocol::bittorrent::message::types::BtMessage::Have {
                                    piece_index,
                                } => connection.update_peer_bitfield(piece_index as usize, 1),
                                aria2_protocol::bittorrent::message::types::BtMessage::Bitfield {
                                    data,
                                } => connection.set_peer_bitfield(&data),
                                aria2_protocol::bittorrent::message::types::BtMessage::HaveAll => {
                                    connection.mark_seeder()
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::HaveNone => {
                                    connection.set_peer_bitfield(&[])
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::Choke => {
                                    connection.stats.peer_choking = true;
                                }
                                aria2_protocol::bittorrent::message::types::BtMessage::Unchoke => {
                                    connection.stats.peer_choking = false;
                                }
                                _ => {}
                            }
                            let after = connection
                                .session_resource
                                .as_ref()
                                .map(|resource| resource.bitfield().to_vec());
                            if before != after
                                && let (Some(peer_key), Some(bitfield)) =
                                    (peer_key, after.as_deref())
                            {
                                peer_tracker.update_peer_bitfield(
                                    &BtPeerInteraction::peer_tracker_key(connection),
                                    bitfield,
                                );
                                peer_last_data_time.insert(peer_key, Instant::now());
                            }
                            None
                        }
                        Ok(None) | Err(_) => {
                            connection.disconnected_gracefully = true;
                            connection.remote_endpoint()
                        }
                    }
                };
                if let Some(address) = failed_address {
                    Self::remove_failed_peers(
                        active_connections,
                        &[address],
                        choking_algo,
                        pex_enabled_peers,
                        peer_last_data_time,
                        allowed_fast_sent_peers,
                        suggest_sent_counts,
                        endgame_state,
                        peer_tracker,
                        peer_storage,
                    );
                }
            }
            PeerWaitEvent::Wake => {}
        }
        None
    }

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
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
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
            .filter_map(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port))
            .collect();
        endgame_state.remove_peers(&removed_keys);
        let mut removed = Vec::new();
        active_connections.retain(|conn| {
            let address =
                match format!("{}:{}", conn.ip_addr, conn.port).parse::<std::net::SocketAddr>() {
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
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for address in &removed {
                storage.return_peer_by_endpoint(&address.ip().to_string(), address.port());
            }
        }
        for peer_key in &removed_keys {
            pex_enabled_peers.remove(peer_key);
            peer_last_data_time.remove(peer_key);
            allowed_fast_sent_peers.remove(peer_key);
            suggest_sent_counts.remove(peer_key);
        }
    }

    fn append_new_connections(
        active_connections: &mut Vec<BtPeerConn>,
        mut new_connections: Vec<BtPeerConn>,
        max_peers: usize,
        is_private: bool,
        context: &mut NewPeerConnectionsContext<'_>,
        peer_storage: &std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
        caretaker_id: u64,
    ) -> usize {
        new_connections.retain(|conn| {
            let Some(endpoint) = conn.remote_endpoint() else {
                tracing::debug!("[BT] Dropping new peer without a remote endpoint");
                return false;
            };
            if endpoint.ip().is_unspecified() || endpoint.port() == 0 {
                tracing::debug!(peer = %endpoint, "[BT] Dropping new peer with invalid endpoint");
                return false;
            }
            true
        });
        let checkout_limit = if max_peers == 0 {
            usize::MAX
        } else {
            max_peers.saturating_sub(active_connections.len())
        };
        let mut seen_endpoints = HashSet::with_capacity(new_connections.len());
        new_connections.retain(|conn| {
            let Some(endpoint) = conn.remote_endpoint() else {
                return false;
            };
            seen_endpoints.insert((endpoint.ip(), endpoint.port()))
        });

        let mut checked_out_endpoints = Vec::with_capacity(new_connections.len());
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            new_connections.retain(|conn| {
                let Some(endpoint) = conn.remote_endpoint() else {
                    return false;
                };
                let entry = crate::engine::bt_peer_storage::PeerEntry::new(
                    endpoint.ip().to_string(),
                    endpoint.port(),
                );
                if checked_out_endpoints.len() >= checkout_limit
                    || storage.add_and_checkout_peer(entry, caretaker_id).is_none()
                {
                    return false;
                }
                checked_out_endpoints.push((endpoint.ip().to_string(), endpoint.port()));
                true
            });
        }

        let previous_len = active_connections.len();
        active_connections.extend(new_connections);
        let connected = active_connections.len() - previous_len;
        {
            let mut storage = peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (ip, port) in &checked_out_endpoints {
                storage.set_peer_active(ip, *port, true);
            }
        }
        if connected == 0 {
            return 0;
        }

        tracing::debug!(connected, "[BT] Added new peer connections");

        for conn in &active_connections[previous_len..] {
            let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port) else {
                continue;
            };
            context.peer_last_data_time.insert(peer_key, Instant::now());
            context.allowed_fast_sent_peers.entry(peer_key).or_default();
            context.suggest_sent_counts.entry(peer_key).or_insert(0);
            if !is_private {
                context.pex_enabled_peers.insert(peer_key);
            }
            let bitfield = conn
                .session_resource
                .as_ref()
                .map_or(&[][..], |resource| resource.bitfield());
            context
                .peer_tracker
                .update_peer_bitfield(&BtPeerInteraction::peer_tracker_key(conn), bitfield);
        }

        if let Some(algo) = context.choking_algo.as_mut() {
            for conn in &active_connections[previous_len..] {
                algo.add_peer(conn.stats.clone());
            }
        }
        connected
    }

    #[allow(clippy::too_many_arguments)]
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
        verified_piece_indices: &[usize],
    ) -> Result<()> {
        // A complete integrity result or bt-seed-unverified path has no piece
        // writes to perform. Return before opening a writer, which could
        // truncate or otherwise rewrite an already-complete payload.
        if verified_piece_indices.len() == num_pieces as usize {
            info!("[BT] All torrent pieces are already complete; skipping piece writer");
            return Ok(());
        }

        // Single-file torrents are written with a positioned + cached writer:
        // BT downloads pieces out of order (RarestFirst etc.), so writes must
        // target the piece offset — the old sequential `write()` appended
        // pieces in arrival order and silently corrupted the file whenever
        // pieces did not arrive in index order. The write-back cache also
        // coalesces adjacent pieces before flushing (C++ WrDiskCache usage).
        // Multi-file torrents go through the coalesced per-file writer below.
        let cache_size_bytes = self.group.recover().options().disk_cache_size_bytes();
        let raw_writer: Box<dyn SeekableDiskWriter> = if self.multi_file_layout.is_none() {
            Box::new(CachedDiskWriter::new_with_mmap_bytes(
                &self.output_path,
                Some(total_size),
                cache_size_bytes,
                false,
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
        // aria2_original uses RarestPieceSelector as the base BitTorrent
        // selector. `bt-prioritize-piece` is an additive wrapper around it,
        // not a replacement for the torrent-wide selection strategy.
        piece_picker.set_strategy(
            aria2_protocol::bittorrent::piece::picker::PieceSelectionStrategy::RarestFirst,
        );

        let allowed_pieces = {
            let group = self.group.recover();
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

        if !self.check_integrity
            && let Some(bitfield) = self
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
            let group = self.group.recover();
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

        let mut peer_tracker =
            aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker::new(num_pieces);
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
        piece_selector.initialize_frequencies(&mut piece_picker, &peer_tracker);

        tracing::info!(
            "[BT] Piece selection strategy: {:?}, {} pieces total, {} peers tracked",
            piece_picker.priority_mode(),
            num_pieces,
            peer_tracker.peer_count()
        );

        // Phase 14 - B1: Initialize endgame state for this download session
        let mut endgame_state = EndgameState::new();
        let request_timeout = {
            let group = self.group.recover();
            Duration::from_secs(group.options().bt_request_timeout.max(1))
        };

        // G1: Snub detection state - track last data received time per peer index
        let mut peer_last_data_time: HashMap<PeerKey, Instant> = HashMap::new();
        let mut last_snub_check = Instant::now();
        let mut stop_timeout = BtStopTimeoutState::new(Instant::now(), self.completed_bytes);

        // Initialize last-data-time tracking for all active peers
        for conn in active_connections.iter() {
            if let Some(key) = PeerKey::from_peer(&conn.ip_addr, conn.port) {
                peer_last_data_time.insert(key, Instant::now());
            }
        }

        loop {
            self.drain_incoming_peers(active_connections, piece_length, total_size);
            self.bt_runtime.set_connections(active_connections.len());

            let (halt_requested, stop_timeout_elapsed) = {
                let group = self.group.recover();
                let halt_requested = group.is_force_halt_requested() || group.is_halt_requested();
                let stop_timeout_elapsed = !halt_requested
                    && stop_timeout.should_halt(
                        group.options().bt_stop_timeout,
                        self.completed_bytes,
                        Instant::now(),
                    );
                (halt_requested, stop_timeout_elapsed)
            };
            if stop_timeout_elapsed {
                let group = self.group.recover();
                let timeout_seconds = group.options().bt_stop_timeout.unwrap_or_default();
                warn!(
                    gid = group.gid().value(),
                    timeout_seconds,
                    "Stopping BitTorrent download after consecutive no-progress timeout"
                );
                group.request_force_halt(HaltReason::Timeout);
                group.set_last_error(DownloadResultCode::TimeOut, "Download timed out");
                continue;
            };
            if halt_requested {
                writer.flush().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to flush halted BT output: {error}"))
                })?;
                writer.close().await.map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                })?;
                if let Some(checkpoint) = self.checkpoint.as_mut() {
                    checkpoint
                        .save(&piece_picker.export_bitfield(), self.completed_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to save halted BT checkpoint: {error}"
                            ))
                        })?;
                    self.group.recover().take_save_control_file_request();
                }
                return Err(Aria2Error::DownloadFailed(
                    "BitTorrent download halted".into(),
                ));
            }
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
            {
                let group = self.group.recover();
                sync_peer_snapshots(&group, active_connections);
            }

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
                    .connect_to_discovered_peers(
                        &all_new_pex_peers,
                        &meta.info_hash.bytes,
                        num_pieces,
                        active_connections,
                        piece_length,
                        total_size,
                    )
                    .await;
                let mut context = NewPeerConnectionsContext {
                    peer_last_data_time: &mut peer_last_data_time,
                    pex_enabled_peers,
                    allowed_fast_sent_peers: &mut self.allowed_fast_sent_peers,
                    suggest_sent_counts: &mut self.suggest_sent_counts,
                    peer_tracker: &mut peer_tracker,
                    choking_algo: &mut self.choking_algo,
                };
                let connected = Self::append_new_connections(
                    active_connections,
                    new_connections,
                    self.group.recover().options().bt_max_peers,
                    self.is_private,
                    &mut context,
                    &self.peer_storage,
                    self.group.recover().gid().value(),
                );
                if connected > 0 {
                    info!("[PEX] Successfully connected to {} new peers", connected);
                    let group = self.group.recover();
                    sync_peer_snapshots(&group, active_connections);
                }
            }

            // Keep tracker numwant aligned with the live peer command count,
            // then re-announce when the tracker interval permits it.
            self.update_tracker_peer_state(active_connections.len());
            if self.should_discover_more_peers(active_connections.len()) {
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
                        .connect_to_discovered_peers(
                            &new_peers,
                            &meta.info_hash.bytes,
                            num_pieces,
                            active_connections,
                            piece_length,
                            total_size,
                        )
                        .await;
                    let mut context = NewPeerConnectionsContext {
                        peer_last_data_time: &mut peer_last_data_time,
                        pex_enabled_peers,
                        allowed_fast_sent_peers: &mut self.allowed_fast_sent_peers,
                        suggest_sent_counts: &mut self.suggest_sent_counts,
                        peer_tracker: &mut peer_tracker,
                        choking_algo: &mut self.choking_algo,
                    };
                    let connected = Self::append_new_connections(
                        active_connections,
                        new_connections,
                        self.group.recover().options().bt_max_peers,
                        self.is_private,
                        &mut context,
                        &self.peer_storage,
                        self.group.recover().gid().value(),
                    );
                    if connected > 0 {
                        info!("[BT] Connected to {} new peers", connected);
                        let group = self.group.recover();
                        sync_peer_snapshots(&group, active_connections);
                    }
                }
            }

            // DHTGetPeersCommand counterpart. The lookup runs in a background
            // task and publishes its result through an event slot, so DHT
            // timeouts do not stall piece scheduling or halt detection.
            self.dht_periodic_lookup
                .set_peer_limits(self.bt_runtime.min_peers(), self.bt_runtime.max_peers());
            let mut dht_peers = Vec::new();
            super::check_periodic_dht_lookup(
                &mut self.dht_periodic_lookup,
                self.dht_engine.as_ref(),
                &meta.info_hash.bytes,
                active_connections.len(),
                &mut dht_peers,
            )
            .await;
            dht_peers.retain(|peer| !self.is_peer_temporarily_rejected(&peer.ip));
            if !dht_peers.is_empty() {
                info!(
                    discovered = dht_peers.len(),
                    "[BT] Periodic DHT lookup found new peers"
                );
                let new_connections = self
                    .connect_to_discovered_peers(
                        &dht_peers,
                        &meta.info_hash.bytes,
                        num_pieces,
                        active_connections,
                        piece_length,
                        total_size,
                    )
                    .await;
                let mut context = NewPeerConnectionsContext {
                    peer_last_data_time: &mut peer_last_data_time,
                    pex_enabled_peers,
                    allowed_fast_sent_peers: &mut self.allowed_fast_sent_peers,
                    suggest_sent_counts: &mut self.suggest_sent_counts,
                    peer_tracker: &mut peer_tracker,
                    choking_algo: &mut self.choking_algo,
                };
                let connected = Self::append_new_connections(
                    active_connections,
                    new_connections,
                    self.group.recover().options().bt_max_peers,
                    self.is_private,
                    &mut context,
                    &self.peer_storage,
                    self.group.recover().gid().value(),
                );
                if connected > 0 {
                    info!("[BT] Connected to {} DHT-discovered peers", connected);
                    let group = self.group.recover();
                    sync_peer_snapshots(&group, active_connections);
                }
            }
            if self.dht_periodic_lookup.is_lookup_completion_pending() {
                self.dht_periodic_lookup
                    .on_lookup_completed(self.tracked_peer_count());
            }

            // With no connected peers, keep the torrent alive for tracker,
            // DHT, PEX, or incoming-peer discovery. The wait is driven by a
            // socket/message event, a lifecycle notification, a completed DHT
            // lookup, or the next protocol/stop-timeout deadline.
            if active_connections.is_empty() && web_seed_manager.is_none() {
                debug!("[BT] No peers available, waiting for peer discovery...");
                let deadline =
                    self.next_peer_event_deadline(active_connections, stop_timeout.deadline());
                let event = self.wait_for_peer_event(active_connections, deadline).await;
                let incoming = Self::apply_peer_wait_event(
                    event,
                    active_connections,
                    &mut peer_tracker,
                    pex_enabled_peers,
                    &mut peer_last_data_time,
                    &mut self.allowed_fast_sent_peers,
                    &mut self.suggest_sent_counts,
                    &mut endgame_state,
                    self.choking_algo.as_mut(),
                    &self.peer_storage,
                );
                if let Some(incoming) = incoming {
                    self.admit_incoming_peer(
                        active_connections,
                        incoming,
                        piece_length,
                        total_size,
                    );
                }
                Self::send_due_keepalives(active_connections).await;
                continue;
            }

            let remaining = piece_picker.remaining_count();
            let selection = piece_selector.select_next_piece(&mut piece_picker, remaining);

            let next_piece_idx = match selection.piece_index {
                Some(idx) => idx,
                None => {
                    tracing::debug!("[BT] No piece available, waiting...");
                    let deadline =
                        self.next_peer_event_deadline(active_connections, stop_timeout.deadline());
                    let event = self.wait_for_peer_event(active_connections, deadline).await;
                    let incoming = Self::apply_peer_wait_event(
                        event,
                        active_connections,
                        &mut peer_tracker,
                        pex_enabled_peers,
                        &mut peer_last_data_time,
                        &mut self.allowed_fast_sent_peers,
                        &mut self.suggest_sent_counts,
                        &mut endgame_state,
                        self.choking_algo.as_mut(),
                        &self.peer_storage,
                    );
                    if let Some(incoming) = incoming {
                        self.admit_incoming_peer(
                            active_connections,
                            incoming,
                            piece_length,
                            total_size,
                        );
                    }
                    Self::send_due_keepalives(active_connections).await;
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
            let max_attempts = self.group.recover().options().max_retries;

            // Phase 14 - B1: Use endgame-aware download when in endgame mode
            // A block read can otherwise wait for the full protocol timeout
            // after pause/remove. Keep the low-level message handler focused
            // on peer I/O and let the owning RequestGroup interrupt the whole
            // piece future through its lifecycle notification.
            let lifecycle_notify = self.group.recover().lifecycle_notifier();
            let lifecycle_wait = lifecycle_notify.notified();
            tokio::pin!(lifecycle_wait);
            lifecycle_wait.as_mut().enable();
            let piece_download = async {
                if endgame_state.is_endgame_active() {
                    info!(
                        "[BT] Endgame: downloading piece {} with duplicate requests ({} peers available)",
                        next_piece_idx,
                        active_connections.len()
                    );
                    BtMessageHandler::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts(
                        active_connections,
                        next_piece_idx as u32,
                        actual_piece_len,
                        num_blocks,
                        &mut endgame_state,
                        self.dht_engine.clone(),
                        Some(self.progress.as_ref()),
                        request_timeout,
                        max_attempts,
                    )
                    .await
                } else {
                    BtMessageHandler::download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts(
                        active_connections,
                        next_piece_idx as u32,
                        actual_piece_len,
                        num_blocks,
                        self.dht_engine.clone(),
                        Some(self.progress.as_ref()),
                        request_timeout,
                        max_attempts,
                    )
                    .await
                }
            };
            let download_result = tokio::select! {
                result = piece_download => result,
                _ = &mut lifecycle_wait => {
                    let halt_requested = {
                        let group = self.group.recover();
                        group.is_force_halt_requested() || group.is_halt_requested()
                    };
                    if !halt_requested {
                        // Save-session and other non-terminal lifecycle
                        // updates share this notifier. Retry the interrupted
                        // piece so its normal completion boundary can consume
                        // the requested checkpoint.
                        continue;
                    }

                    writer.flush().await.map_err(|error| {
                        Aria2Error::FileIo(format!("Failed to flush halted BT output: {error}"))
                    })?;
                    writer.close().await.map_err(|error| {
                        Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
                    })?;
                    if let Some(checkpoint) = self.checkpoint.as_mut() {
                        checkpoint
                            .save(&piece_picker.export_bitfield(), self.completed_bytes)
                            .await
                            .map_err(|error| {
                                Aria2Error::FileIo(format!(
                                    "Failed to save halted BT checkpoint: {error}"
                                ))
                            })?;
                        self.group.recover().take_save_control_file_request();
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
                        let Some(conn) = active_connections.get(peer_download.peer_index) else {
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
                        active_connections[peer_download.peer_index]
                            .stats
                            .on_data_received(peer_download.bytes);
                        self.on_data_received_from_peer(
                            peer_download.peer_index,
                            peer_download.bytes,
                        );
                        peer_last_data_time.insert(peer_key, Instant::now());
                    }

                    Self::remove_failed_peers(
                        active_connections,
                        &piece_result.failed_peers,
                        self.choking_algo.as_mut(),
                        pex_enabled_peers,
                        &mut peer_last_data_time,
                        &mut self.allowed_fast_sent_peers,
                        &mut self.suggest_sent_counts,
                        &mut endgame_state,
                        &mut peer_tracker,
                        &self.peer_storage,
                    );
                    self.update_tracker_peer_state(active_connections.len());
                    {
                        let group = self.group.recover();
                        sync_peer_snapshots(&group, active_connections);
                    }

                    tracing::info!(
                        "[BT] All blocks received for piece {}, verifying...",
                        next_piece_idx
                    );
                    let expected_hash = piece_manager.expected_piece_hash(next_piece_idx as u32);
                    let (piece_verified, piece_data) =
                        super::verify_piece_hash_async(expected_hash, piece_data).await?;
                    if piece_verified {
                        tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);

                        let piece_bytes = bytes::Bytes::from(piece_data);
                        if let Some(ref layout) = self.multi_file_layout {
                            let max_open_files = self.group.recover().options().bt_max_open_files;
                            crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced_with_limit(
                                layout,
                                next_piece_idx as u32,
                                &piece_bytes,
                                layout.piece_length(),
                                max_open_files,
                            )
                            .await?;
                        } else {
                            writer
                                .write_bytes_at(
                                    next_piece_idx as u64 * piece_length as u64,
                                    piece_bytes,
                                )
                                .await?;
                        }

                        self.completed_bytes += piece_data_len as u64;

                        // Sync bitfield to RequestGroup for session persistence
                        let bitfield = piece_picker.export_bitfield();
                        {
                            let g = self.group.recover();
                            g.set_bt_bitfield(Some(bitfield.clone()));
                        }
                        self.persist_checkpoint_after_piece(
                            &mut writer,
                            &bitfield,
                            piece_data_len as u64,
                        )
                        .await?;

                        BtPeerInteraction::broadcast_have(
                            active_connections,
                            next_piece_idx as u32,
                        )
                        .await;
                        piece_ok = true;

                        // P1 integration: periodically save download progress
                        self.maybe_save_progress(
                            meta,
                            &bitfield,
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
                        let mut peer_bytes = piece_result.peer_bytes.iter();
                        let unique_peer = peer_bytes
                            .next()
                            .filter(|first| peer_bytes.all(|peer| peer.peer == first.peer));
                        if let Some(peer_download) = unique_peer {
                            let peer_ip = peer_download.peer.ip().to_string();
                            self.reject_peer_temporarily(&peer_ip);
                            tracing::warn!(
                                peer = %peer_download.peer,
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
                        max_attempts
                    );
                    return Err(Aria2Error::Fatal(FatalError::Config(format!(
                        "Piece {} download failed after {} retries",
                        next_piece_idx, max_attempts
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
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.remove().await?;
        }
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
        bitfield: &[u8],
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
            let progress = progress_snapshot(
                meta.info_hash.bytes,
                bitfield,
                piece_length,
                total_size,
                num_pieces,
                ProgressDownloadStats {
                    downloaded_bytes: self.completed_bytes,
                    uploaded_bytes: self.total_uploaded,
                    upload_speed: 0.0,
                    download_speed: 0.0,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
            );

            match mgr.save_progress(&meta.info_hash.bytes, &progress) {
                Ok(()) => {
                    *last_progress_save = Instant::now();
                    debug!(
                        pieces_completed = next_piece_idx + 1,
                        total_pieces = num_pieces,
                        "BT progress saved successfully"
                    );
                }
                Err(e) => warn!(error = %e, "Failed to save BT progress"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BtStopTimeoutState, ProgressDownloadStats, progress_snapshot};
    use std::time::{Duration, Instant};

    #[test]
    fn zero_timeout_is_disabled() {
        let start = Instant::now();
        let mut state = BtStopTimeoutState::new(start, 0);

        assert!(!state.should_halt(Some(0), 0, start + Duration::from_secs(60)));
        assert!(!state.should_halt(None, 0, start + Duration::from_secs(120)));
    }

    #[test]
    fn completed_piece_progress_resets_timeout_checkpoint() {
        let start = Instant::now();
        let mut state = BtStopTimeoutState::new(start, 0);

        assert!(!state.should_halt(Some(2), 0, start));
        assert!(!state.should_halt(Some(2), 0, start + Duration::from_secs(1)));
        assert!(!state.should_halt(Some(2), 1, start + Duration::from_secs(1)));
        assert!(!state.should_halt(Some(2), 1, start + Duration::from_secs(2)));
        assert!(state.should_halt(Some(2), 1, start + Duration::from_secs(3)));
    }

    #[test]
    fn progress_snapshot_preserves_completed_bitfield() {
        let snapshot = progress_snapshot(
            [0x11; 20],
            &[0b1100_0000],
            4,
            8,
            2,
            ProgressDownloadStats {
                downloaded_bytes: 8,
                uploaded_bytes: 3,
                upload_speed: 0.0,
                download_speed: 0.0,
                elapsed_seconds: 9,
            },
        );

        assert_eq!(snapshot.bitfield, vec![0b1100_0000]);
        assert_eq!(snapshot.piece_length, 4);
        assert_eq!(snapshot.total_size, 8);
        assert_eq!(snapshot.num_pieces, 2);
        assert_eq!(snapshot.upload_length, 3);
        assert_eq!(snapshot.stats.downloaded_bytes, 8);
    }
}
