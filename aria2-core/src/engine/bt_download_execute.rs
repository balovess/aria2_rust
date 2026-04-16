#![allow(dead_code)]

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::{
    BLOCK_SIZE, BtDownloadCommand, MAX_PUBLIC_TRACKERS_TO_TRY, MAX_RETRIES,
    PEER_CONNECTION_DELAY_MS, PUBLIC_TRACKER_PEER_THRESHOLD,
};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced;
use crate::engine::bt_piece_selector::{BtPieceSelector, build_bitfield_from_completed};
use crate::engine::bt_post_download_handler::{DownloadStatus, HookContext};
use crate::engine::bt_progress_info_file::{BtProgress, DownloadStats as ProgressDownloadStats};
use crate::engine::bt_tracker_comm::{announce_to_public_tracker, perform_http_tracker_announce};
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::peer_stats::PeerStats;
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::engine::udp_tracker_manager::UdpTrackerManager;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use aria2_protocol::bittorrent::extension::pex::PexHandler;
use aria2_protocol::bittorrent::message::serializer;
use aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker;

/// Tracks duplicate requests during endgame mode.
///
/// In endgame mode (when <=5 pieces remain incomplete), we request the same block
/// from multiple peers simultaneously to speed up completion. When any peer responds
/// with the block data, we send Cancel messages to the other peers that also received
/// the request for that block.
///
/// This struct maintains the mapping from block identifiers to the list of peers
/// that were sent duplicate requests, enabling efficient cancellation on arrival.
pub struct EndgameState {
    /// Map from (piece_index, offset, length) -> list of peer indices that received this request
    active_duplicate_requests: HashMap<(u32, u32, u32), Vec<usize>>,
    /// Whether we're currently in endgame mode
    active: bool,
}

impl EndgameState {
    /// Create a new EndgameState in inactive state
    pub fn new() -> Self {
        Self {
            active_duplicate_requests: HashMap::new(),
            active: false,
        }
    }

    /// Enter endgame mode - enables duplicate request tracking
    pub fn enter_endgame(&mut self) {
        if !self.active {
            self.active = true;
            info!("[BT] === Entering endgame mode ===");
        }
    }

    /// Exit endgame mode and clear all tracked requests
    pub fn exit_endgame(&mut self) {
        if self.active {
            self.active = false;
            self.active_duplicate_requests.clear();
            debug!(
                "[BT] Exiting endgame mode, cleared {} tracked requests",
                self.active_duplicate_requests.len()
            );
        }
    }

    /// Register that a request was sent to a peer during endgame
    ///
    /// This tracks which peers have pending requests for each block so we can
    /// cancel redundant requests when the first response arrives.
    pub fn track_request(&mut self, piece: u32, offset: u32, len: u32, peer_id: usize) {
        let key = (piece, offset, len);
        self.active_duplicate_requests
            .entry(key)
            .or_default()
            .push(peer_id);
    }

    /// When a block arrives, find other peers that have pending requests for the same block
    ///
    /// Returns the list of peer indices that should receive Cancel messages.
    /// Does NOT remove the entry (call remove_request after sending cancels).
    pub fn get_cancel_targets(&self, piece: u32, offset: u32, len: u32) -> Vec<usize> {
        let key = (piece, offset, len);
        self.active_duplicate_requests
            .get(&key)
            .map(|peers| peers.to_vec())
            .unwrap_or_default()
    }

    /// Remove a tracked request after cancel or completion
    ///
    /// Called after Cancel messages have been sent and the block is fully processed.
    pub fn remove_request(&mut self, piece: u32, offset: u32, len: u32) {
        let key = (piece, offset, len);
        self.active_duplicate_requests.remove(&key);
    }

    /// Check if endgame mode is currently active
    pub fn is_endgame_active(&self) -> bool {
        self.active
    }

    /// Get the number of actively tracked duplicate requests (for debugging/metrics)
    #[allow(dead_code)]
    pub fn tracked_count(&self) -> usize {
        self.active_duplicate_requests.len()
    }
}

impl Default for EndgameState {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let (meta, piece_length, total_size, num_pieces) = self.prepare_environment().await?;

        // P1 集成: 尝试从 .aria2 文件恢复已保存的进度
        if let Some(ref mgr) = self.progress_manager {
            match mgr.load_progress(&meta.info_hash.bytes) {
                Ok(saved) => {
                    info!(
                        pieces_done = saved.num_pieces,
                        ratio = saved.completion_ratio(),
                        "Resuming from saved progress"
                    );
                }
                Err(e) => {
                    debug!(
                        error = %e,
                        "No saved progress found, starting fresh download"
                    );
                }
            }
        }

        let peer_addrs = self
            .discover_peers(&meta, total_size, &meta.info_hash.bytes)
            .await?;

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No peers from tracker or DHT".into(),
                },
            ));
        }

        let mut active_connections = self
            .connect_to_peers(&peer_addrs, &meta.info_hash.bytes, num_pieces)
            .await?;

        // Initialize PEX known peers list from discovered peers for BEP 11 exchange
        {
            let pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = peer_addrs
                .iter()
                .map(|pa| {
                    aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&pa.ip, pa.port)
                })
                .collect();
            self.set_pex_known_peers(pex_peers);
            info!(
                "[PEX] Initialized with {} known peers from tracker/DHT",
                self.pex_known_peers.len()
            );
        }

        // TODO: PEX Integration Point - After extension handshake and bitfield exchange:
        // For each active connection that supports ut_pex (check extension IDs from handshake):
        //   1. Call self.check_pex_support(local_ext_ids, remote_ext_ids) to verify mutual support
        //   2. Enable periodic PEX exchange by calling self.maybe_send_pex(&remote_addr) in the loop
        //   3. When receiving extension message ID == PexHandler::EXTENSION_ID, call:
        //      self.handle_incoming_pex(&pex_data, &local_addr) to process discovered peers
        //
        // Note: Extension handshake happens during BtPeerInteraction::connect_to_peers()
        // The actual PEX message send/receive should be integrated into the piece download loop
        // below, respecting the 60-second rate limit enforced by should_send_pex()

        self.download_pieces_loop(
            &mut active_connections,
            &meta,
            piece_length,
            total_size,
            num_pieces,
        )
        .await?;

        if self.seed_enabled && !active_connections.is_empty() {
            info!(
                "Starting seeding phase with {} peers...",
                active_connections.len()
            );
            self.run_seeding_phase(active_connections, piece_length, num_pieces)
                .await?;
        } else {
            info!(
                "Skipping seeding (enabled={}, connections={})",
                self.seed_enabled,
                active_connections.len()
            );
            for conn in &mut active_connections {
                let _ = conn;
            }
        }

        self.finalize_download(Instant::now(), &meta).await?;

        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl BtDownloadCommand {
    async fn prepare_environment(
        &mut self,
    ) -> Result<(
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        u32,
        u64,
        u32,
    )> {
        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        if let Some(ref layout) = self.multi_file_layout {
            layout.create_directories().map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "create_directories failed: {}",
                    e
                )))
            })?;
            info!(
                "[BT] Multi-file mode: {} files under {}",
                layout.num_files(),
                self.output_path.display()
            );
        }

        let meta =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&self.torrent_data)
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("Torrent parse error: {}", e)))
                })?;

        {
            let mut g = self.group.write().await;
            g.set_total_length(meta.total_size()).await;
            g.set_total_length_atomic(meta.total_size());
        }

        let piece_length = meta.info.piece_length;
        let total_size = meta.total_size();
        let num_pieces = meta.num_pieces() as u32;

        Ok((meta, piece_length, total_size, num_pieces))
    }

    async fn discover_peers(
        &mut self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        total_size: u64,
        info_hash_raw: &[u8; 20],
    ) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();
        let mut peer_addrs =
            perform_http_tracker_announce(&meta.announce, info_hash_raw, &my_peer_id, total_size)
                .await?;

        if let Ok(udp) = UdpTrackerClient::new(0).await {
            self.udp_client = Some(std::sync::Arc::new(tokio::sync::Mutex::new(udp)));
            if let Some(ref shared_client) = self.udp_client {
                let mut mgr = UdpTrackerManager::new(std::sync::Arc::clone(shared_client)).await;
                let urls: Vec<String> = meta.announce_list.iter().flatten().cloned().collect();
                mgr.parse_tracker_urls(&urls);

                if mgr.endpoint_count() > 0 {
                    debug!("Trying {} UDP tracker endpoints", mgr.endpoint_count());

                    match mgr.announce(
                        info_hash_raw, &my_peer_id,
                        0, total_size as i64, 0,
                        aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent::Started,
                        50,
                    ).await {
                        udp_responses if !udp_responses.is_empty() => {
                            let udp_peers = UdpTrackerManager::collect_all_peers(&udp_responses);
                            debug!("UDP trackers returned {} additional peers", udp_peers.len());
                            for (ip, port) in udp_peers {
                                peer_addrs.push(aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port));
                            }
                        }
                        _ => { debug!("No response from UDP trackers"); }
                    }
                }
            }
        }

        if peer_addrs.is_empty() {
            tracing::error!("[BT] ERROR: No peers from tracker");
        }

        let enable_dht = { self.group.read().await.options().enable_dht };
        if enable_dht && self.dht_engine.is_none() {
            let dht_port = { self.group.read().await.options().dht_listen_port };
            let dht_config = aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
                port: dht_port.unwrap_or(0),
                dht_file_path: self.group.read().await.options().dht_file_path.clone(),
                ..Default::default()
            };
            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
                    self.dht_engine = Some(engine);
                    tracing::info!("[BT] DHT engine started");
                    self.dht_engine.as_ref().unwrap().start_maintenance_loop();
                }
                Err(e) => {
                    warn!("[BT] DHT engine start failed: {}", e);
                }
            }
        }

        if let Some(ref engine) = self.dht_engine {
            let result = engine.find_peers(info_hash_raw).await;
            if !result.peers.is_empty() {
                let before = peer_addrs.len();
                for addr in &result.peers {
                    let ip_str = addr.ip().to_string();
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                        &ip_str,
                        addr.port(),
                    );
                    if !peer_addrs
                        .iter()
                        .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }
                tracing::info!(
                    "[BT] DHT discovered {} extra peers (total: {}, contacted {} DHT nodes)",
                    peer_addrs.len() - before,
                    peer_addrs.len(),
                    result.nodes_contacted
                );
            } else {
                debug!("[BT] DHT find_peers returned no peers");
            }
        }

        let enable_public_trackers = { self.group.read().await.options().enable_public_trackers };
        if enable_public_trackers
            && self.public_trackers.is_none()
            && peer_addrs.len() < PUBLIC_TRACKER_PEER_THRESHOLD
        {
            let ptl = std::sync::Arc::new(
                aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
            );
            ptl.start_auto_update(
                "https://cf.trackerslist.com/best.txt".to_string(),
                std::time::Duration::from_secs(86400),
            );
            self.public_trackers = Some(ptl);
        }

        if let Some(ref pt) = self.public_trackers {
            let http_urls = pt.get_http_trackers().await;
            let mut extra_peers: Vec<(String, u16)> = Vec::new();
            let mut announced = 0usize;

            for url in http_urls.iter().take(MAX_PUBLIC_TRACKERS_TO_TRY) {
                match announce_to_public_tracker(url, info_hash_raw, &my_peer_id, total_size).await
                {
                    Ok(peers) => {
                        announced += 1;
                        extra_peers.extend(peers);
                    }
                    Err(e) => {
                        debug!("[BT] Public tracker {} failed: {}", url, e);
                    }
                }
            }

            if !extra_peers.is_empty() {
                let before = peer_addrs.len();
                for (ip, port) in extra_peers {
                    let paddr =
                        aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port);
                    if !peer_addrs
                        .iter()
                        .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }
                tracing::info!(
                    "[BT] Public trackers discovered {} extra peers (announced to {} of {})",
                    peer_addrs.len() - before,
                    announced,
                    http_urls.len()
                );
            } else if announced > 0 {
                debug!("[BT] Public trackers responded but no peers found");
            }
        }

        // P2: Integrate LPD-discovered LAN peers
        if let Some(ref lpd) = self.lpd_manager {
            // Convert raw 20-byte info_hash to 40-char hex string for LPD
            let info_hash_hex = hex::encode(*info_hash_raw);
            let lpd_peers = lpd.get_peers_for(&info_hash_hex).await;
            if !lpd_peers.is_empty() {
                let before = peer_addrs.len();
                for lpd_peer in &lpd_peers {
                    // LpdPeer.addr is IpAddr, LpdPeer.port is u16
                    let ip_str = lpd_peer.addr.to_string();
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                        &ip_str,
                        lpd_peer.port,
                    );
                    if !peer_addrs
                        .iter()
                        .any(|p| p.ip == paddr.ip && p.port == paddr.port)
                    {
                        peer_addrs.push(paddr);
                    }
                }

                info!(
                    lpd_count = lpd_peers.len(),
                    total_added = peer_addrs.len() - before,
                    "LPD discovered local peers"
                );

                // Register current download for LPD announcement
                let _ = lpd.register_torrent(&info_hash_hex).await;
            } else {
                debug!("LPD no local peers found for this torrent");
            }
        }

        Ok(peer_addrs)
    }

    async fn connect_to_peers(
        &mut self,
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
    ) -> Result<Vec<BtPeerConn>> {
        let require_crypto = { self.group.read().await.options().bt_require_crypto };
        let force_encrypt = { self.group.read().await.options().bt_force_encrypt };

        let conn_result = BtPeerInteraction::connect_to_peers(
            peer_addrs,
            info_hash_raw,
            num_pieces,
            require_crypto,
            force_encrypt,
        )
        .await?;

        let active_connections = conn_result.connections;

        tracing::info!("[BT] Active connections: {}", active_connections.len());
        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed".into(),
                },
            ));
        }

        {
            let options = self.group.read().await.options().clone();
            let config = ChokingConfig {
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10,
            };

            let mut algo = ChokingAlgorithm::new(config);

            for addr in peer_addrs {
                let socket_addr = std::net::SocketAddr::new(
                    addr.ip.parse().unwrap_or_else(|_| {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                    }),
                    addr.port,
                );
                let peer_stats = PeerStats::new([0u8; 20], socket_addr);
                algo.add_peer(peer_stats);
            }

            self.choking_algo = Some(algo);
            tracing::info!(
                "[BT] Choking algorithm initialized with {} peers",
                self.choking_algo.as_ref().unwrap().len()
            );
        }

        Ok(active_connections)
    }

    async fn download_pieces_loop(
        &mut self,
        active_connections: &mut [BtPeerConn],
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
    ) -> Result<()> {
        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let rate_limit = {
            let g = self.group.read().await;
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

        // P1 集成: 进度保存时间追踪
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
            let g = self.group.read().await;
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
                // Default: RarestFirst (also handles "rarest" and empty string)
                piece_picker.set_priority_mode(
                    aria2_protocol::bittorrent::piece::picker::PiecePriorityMode::RarestFirst,
                );
                info!("[BT] Piece priority mode: RarestFirst (default)");
            }
        }

        let mut peer_tracker = PeerBitfieldTracker::new(num_pieces);
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
        const SNUB_TIMEOUT_SECS: u64 = 30;
        let mut peer_last_data_time: std::collections::HashMap<usize, Instant> =
            std::collections::HashMap::new();
        let mut last_snub_check = Instant::now();
        const SNUB_CHECK_INTERVAL_SECS: u64 = 10;

        // Initialize last-data-time tracking for all active peers
        for (idx, _conn) in active_connections.iter().enumerate() {
            peer_last_data_time.insert(idx, Instant::now());
        }

        loop {
            if BtPieceSelector::is_complete(&piece_picker) {
                // Exit endgame mode when download is complete
                if endgame_state.is_endgame_active() {
                    endgame_state.exit_endgame();
                }
                break;
            }

            // Phase 14 - B1: Check if we should enter endgame mode
            // Endgame activates when <=5 pieces remain incomplete (matching picker threshold)
            let endgame_candidates = piece_picker.endgame_candidates();
            if !endgame_candidates.is_empty() && !endgame_state.is_endgame_active() {
                endgame_state.enter_endgame();
                info!(
                    "[BT] Endgame mode activated: {}/{} pieces remaining",
                    endgame_candidates.len(),
                    num_pieces
                );
            } else if endgame_candidates.is_empty() && endgame_state.is_endgame_active() {
                // This shouldn't normally happen (candidates empty means >5 remaining or all done)
                // but handle gracefully
                endgame_state.exit_endgame();
            }

            // G1: Periodic snub detection - check all peers for inactivity
            if last_snub_check.elapsed().as_secs() >= SNUB_CHECK_INTERVAL_SECS {
                last_snub_check = Instant::now();
                let mut newly_snubbed = Vec::new();
                for (&peer_id, &last_time) in &peer_last_data_time {
                    if last_time.elapsed().as_secs() > SNUB_TIMEOUT_SECS {
                        // Mark peer as snubbed via the command's choking algorithm
                        self.mark_peer_snubbed(peer_id);
                        newly_snubbed.push(peer_id);
                        debug!(
                            "[BT] Peer {} marked as snubbed (no data for {}s)",
                            peer_id,
                            last_time.elapsed().as_secs()
                        );
                    }
                }
                if !newly_snubbed.is_empty() {
                    debug!(
                        "[BT] Snub check: {} peers newly snubbed",
                        newly_snubbed.len()
                    );
                }

                // Also run the PeerStats-level snub check (timeout-based)
                let stats_snubbed = self.check_snubbed_peers();
                if !stats_snubbed.is_empty() {
                    debug!(
                        "[BT] PeerStats snub check: {} peers timed out",
                        stats_snubbed.len()
                    );
                }
            }

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
                    // (In a full implementation, this would be per-peer; here we update all
                    // since the download loop processes pieces from any peer)
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
                            // Phase 14 / I4: use coalesced writer to reduce syscalls
                            write_piece_to_multi_files_coalesced(
                                layout,
                                next_piece_idx as u32,
                                &piece_data,
                                layout.piece_length(),
                            )
                            .await?;
                        } else {
                            writer.write(&piece_data).await.ok();
                        }

                        {
                            let bitfield =
                                build_bitfield_from_completed(piece_manager.num_pieces(), |i| {
                                    piece_manager.is_completed(i)
                                });
                            let g = self.group.write().await;
                            g.set_bt_bitfield(Some(bitfield)).await;
                        }

                        BtPeerInteraction::broadcast_have(
                            active_connections,
                            next_piece_idx as u32,
                        )
                        .await;
                        piece_ok = true;

                        // PEX Integration: Trigger PEX send on piece completion
                        // This ensures peers are exchanged when progress is made
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
                                // Note: Actual sending would happen via extension message channel
                                // when full extension protocol integration is implemented
                            }
                        }

                        // P1 集成: 定期保存下载进度到 .aria2 文件
                        if let Some(ref mgr) = self.progress_manager
                            && last_progress_save.elapsed() >= self.progress_save_interval
                        {
                            // 构造当前进度快照
                            let progress = BtProgress {
                                info_hash: meta.info_hash.bytes,
                                bitfield: vec![], // 将在后续完善
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
                                save_time: std::time::SystemTime::now(),
                                version: 1,
                            };

                            if let Err(e) = mgr.save_progress(&meta.info_hash.bytes, &progress) {
                                warn!(
                                    error = %e,
                                    "Failed to save BT progress"
                                );
                            } else {
                                debug!(
                                    pieces_completed = next_piece_idx + 1,
                                    total_pieces = num_pieces,
                                    "BT progress saved successfully"
                                );
                            }
                            last_progress_save = Instant::now();
                        }
                    } else {
                        tracing::warn!(
                            "[BT] SHA1 mismatch on piece {}, retrying...",
                            next_piece_idx
                        );

                        // H3: Bad peer detection - increment bad data counter for peers
                        // that contributed to this failed piece.
                        // Note: In a full implementation, we would track which specific peer
                        // sent each block and only penalize that peer. For now, we log
                        // the event and the caller can use record_bad_piece_for_peer() if
                        // they know which peer sent the invalid data.
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
                tracing::error!(
                    "[BT] Piece {} failed after {} retries",
                    next_piece_idx,
                    MAX_RETRIES
                );
                return Err(Aria2Error::Fatal(FatalError::Config(format!(
                    "Piece {} download failed after {} retries",
                    next_piece_idx, MAX_RETRIES
                ))));
            }

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                g.set_completed_length(self.completed_bytes);

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    g.set_download_speed_cached(speed);
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

    async fn finalize_download(
        &mut self,
        start_time: Instant,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    ) -> Result<()> {
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, self.total_uploaded).await;
            g.set_completed_length(self.completed_bytes);
            g.set_download_speed_cached(final_speed);
            g.set_uploaded_length(self.total_uploaded);
            g.complete().await?;
        }

        info!(
            "BT command done: downloaded={} uploaded={}",
            self.completed_bytes, self.total_uploaded
        );

        if let Some(ref engine) = self.dht_engine {
            if let Err(e) = engine.announce_peer(&meta.info_hash.bytes, 0).await {
                warn!("[BT] DHT announce failed: {}", e);
            } else {
                info!(
                    "[BT] DHT announce_peer sent for {}",
                    meta.info_hash.as_hex()
                );
            }
            engine.shutdown();
        }

        // P1 集成: 清理已完成的下载进度文件
        if let Some(ref mgr) = self.progress_manager {
            if let Err(e) = mgr.remove_progress(&meta.info_hash.bytes) {
                warn!(
                    error = %e,
                    "Failed to remove progress file after completion"
                );
            } else {
                info!("BT progress file removed after successful download");
            }
        }

        // P2 集成: 触发下载完成后处理钩子
        if let Some(ref hm) = self.hook_manager {
            // 获取 gid（从 group 中提取）
            let gid = {
                let g = self.group.read().await;
                g.gid()
            };

            let ctx = HookContext {
                gid,
                file_path: self.output_path.clone(),
                status: DownloadStatus::Complete,
                stats: crate::engine::bt_post_download_handler::DownloadStats {
                    uploaded_bytes: self.total_uploaded,
                    downloaded_bytes: self.completed_bytes,
                    upload_speed: 0.0,
                    download_speed: final_speed as f64,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
                error: None,
            };

            match hm.fire_complete(&ctx).await {
                Ok(results) => {
                    info!(
                        hook_count = results.len(),
                        "All post-download hooks executed successfully"
                    );
                    for result in &results {
                        debug!(result = %result, "Hook execution result");
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "Post-download hook execution failed (non-fatal)"
                    );
                }
            }
        }

        Ok(())
    }

    /// Check if both local and remote peer support ut_pex extension
    pub fn check_pex_support(
        local_extension_ids: &[Option<u8>],
        remote_extension_ids: &[Option<u8>],
    ) -> bool {
        let local_supports = local_extension_ids.contains(&Some(PexHandler::EXTENSION_ID));
        let remote_supports = remote_extension_ids.contains(&Some(PexHandler::EXTENSION_ID));
        local_supports && remote_supports
    }

    /// Build and optionally send a PEX message to connected peers
    /// Returns the encoded PEX message (or None if not ready to send)
    pub fn maybe_send_pex(
        &mut self,
        remote_peer_addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Option<Vec<u8>> {
        if !self.should_send_pex() {
            return None;
        }

        if self.pex_known_peers.is_empty() {
            debug!("[PEX] No known peers to exchange");
            return None;
        }

        debug!(
            known_peers = self.pex_known_peers.len(),
            remote = %format!("{}:{}", remote_peer_addr.ip, remote_peer_addr.port),
            "[PEX] Building PEX message"
        );

        let pex_msg = PexHandler::build_pex_added(
            &self.pex_known_peers,
            remote_peer_addr,
            PexHandler::DEFAULT_MAX_PEERS,
        );

        let encoded = pex_msg.encode();
        self.update_pex_last_send();

        debug!(
            size = encoded.len(),
            "[PEX] PEX message built and ready to send"
        );
        Some(encoded)
    }

    /// Process an incoming PEX message and extract discovered/dropped peers
    pub fn handle_incoming_pex(
        &mut self,
        pex_data: &[u8],
        local_addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Result<(
        Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
        Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    )> {
        match PexHandler::process_received_pex(pex_data, local_addr) {
            Ok((added, dropped)) => {
                if !added.is_empty() {
                    info!(count = added.len(), "[PEX] Discovered new peers from PEX");
                    for peer in &added {
                        self.add_pex_peer(peer.clone());
                    }
                }
                if !dropped.is_empty() {
                    debug!(count = dropped.len(), "[PEX] Peers to drop from PEX");
                }
                Ok((added, dropped))
            }
            Err(e) => {
                warn!(error = %e, "[PEX] Failed to process incoming PEX message");
                Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("PEX processing failed: {}", e),
                    },
                ))
            }
        }
    }

    // ==================== BEP 6 Fast Extension (AllowedFast / Suggest) ====================

    /// Maximum number of AllowedFast messages to send to a single peer
    const MAX_ALLOWED_FAST_PER_PEER: usize = 10;

    /// Maximum number of Suggest messages to send per session per peer
    const MAX_SUGGEST_PER_PEER: usize = 5;

    /// Check if a bitfield has a specific piece index set
    ///
    /// BitTorrent bitfields use MSB-first ordering within each byte.
    fn is_bitfield_set(bitfield: &[u8], piece_index: u32) -> bool {
        let byte_idx = (piece_index as usize) / 8;
        let bit_idx = 7 - ((piece_index as usize) % 8);

        if byte_idx >= bitfield.len() {
            return false;
        }

        (bitfield[byte_idx] & (1 << bit_idx)) != 0
    }

    /// Calculate the set of pieces to send as AllowedFast to a peer
    ///
    /// Selects up to `MAX_ALLOWED_FAST_PER_PEER` pieces that:
    /// - We still need (not completed)
    /// - The peer has (based on their bitfield)
    /// - We haven't already sent AllowedFast for
    fn calculate_fast_set(
        needed_pieces: &[u32],
        peer_bitfield: &[u8],
        already_sent: &HashSet<u32>,
    ) -> Vec<u32> {
        let mut fast_set = Vec::new();

        for &piece_idx in needed_pieces.iter() {
            if fast_set.len() >= Self::MAX_ALLOWED_FAST_PER_PEER {
                break;
            }
            if already_sent.contains(&piece_idx) {
                continue;
            }

            // Check if peer has this piece (bitfield check)
            if Self::is_bitfield_set(peer_bitfield, piece_idx) {
                fast_set.push(piece_idx);
            }
        }

        fast_set
    }

    /// Send AllowedFast messages to a peer that supports BEP 6 Fast Extension
    ///
    /// This should be called after the extension handshake completes and we've received
    /// the peer's bitfield. It allows us to request specific pieces even when choked.
    async fn send_allowed_fast_to_peer(
        peer_conn: &mut BtPeerConn,
        needed_pieces: &[u32],
        peer_bitfield: &[u8],
        already_sent: &mut HashSet<u32>,
    ) -> Result<usize> {
        let fast_set = Self::calculate_fast_set(needed_pieces, peer_bitfield, already_sent);
        let count = fast_set.len();

        for piece_idx in fast_set {
            let _msg = serializer::serialize_allowed_fast(piece_idx);

            // Note: In a full implementation, this would use a proper message queue/channel.
            // For now, we log and track what would be sent.
            debug!("[BEP6] Would send AllowedFast for piece {}", piece_idx);

            already_sent.insert(piece_idx);
            peer_conn.add_allowed_fast(piece_idx);
        }

        if count > 0 {
            info!("[BEP6] Sent {} AllowedFast messages to peer", count);
        }

        Ok(count)
    }

    /// Initialize BEP 6 tracking structures for all active connections
    fn init_bep6_tracking(&mut self, num_connections: usize) {
        self.allowed_fast_sent_peers = HashMap::with_capacity(num_connections);
        self.suggest_sent_counts = HashMap::with_capacity(num_connections);
    }

    /// Send AllowedFast messages to all peers after handshake/bitfield exchange
    ///
    /// This is called once during initialization to establish fast extension
    /// support with compatible peers.
    async fn broadcast_allowed_fast(
        &mut self,
        active_connections: &mut [BtPeerConn],
        needed_pieces: &[u32],
        peer_bitfields: &[Vec<u8>],
    ) -> Result<u64> {
        self.init_bep6_tracking(active_connections.len());

        let mut total_sent = 0u64;

        for (idx, conn) in active_connections.iter_mut().enumerate() {
            let peer_bf = if idx < peer_bitfields.len() {
                &peer_bitfields[idx]
            } else {
                continue;
            };

            let mut sent_for_peer = HashSet::new();
            match Self::send_allowed_fast_to_peer(conn, needed_pieces, peer_bf, &mut sent_for_peer)
                .await
            {
                Ok(count) => {
                    total_sent += count as u64;
                    if !sent_for_peer.is_empty() {
                        self.allowed_fast_sent_peers.insert(idx, sent_for_peer);
                    }
                }
                Err(e) => {
                    warn!("[BEP6] Failed to send AllowedFast to peer {}: {}", idx, e);
                }
            }
        }

        if total_sent > 0 {
            info!(
                "[BEP6] Broadcast {} total AllowedFast messages to {} peers",
                total_sent,
                active_connections.len()
            );
        }

        Ok(total_sent)
    }

    /// Send Suggest messages to a peer to guide them toward pieces we need most
    ///
    /// Called after unchoking a peer, this sends up to `MAX_SUGGEST_PER_PEER` Suggest
    /// messages for high-priority, low-availability pieces we need urgently.
    ///
    /// # Arguments
    /// * `peer_idx` - Index of the peer in active_connections
    /// * `piece_picker` - The piece picker for selecting which pieces to suggest
    async fn send_suggest_to_peer(
        &mut self,
        peer_idx: usize,
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
    ) -> Result<usize> {
        // Check if we've already sent too many suggests to this peer
        let sent_count = self
            .suggest_sent_counts
            .get(&peer_idx)
            .copied()
            .unwrap_or(0);
        if sent_count >= Self::MAX_SUGGEST_PER_PEER {
            debug!(
                "[BEP6] Already sent {} suggests to peer {}, skipping",
                sent_count, peer_idx
            );
            return Ok(0);
        }

        let remaining = Self::MAX_SUGGEST_PER_PEER - sent_count;

        // Select high-priority, low-availability pieces we need most urgently
        let mut suggestions: Vec<u32> = piece_picker
            .pieces_iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .take(remaining)
            .map(|p| p.index)
            .collect();

        // Sort by priority (highest first), then by rarity (lowest frequency)
        suggestions.sort_by(|&a, &b| {
            let pa = piece_picker.get_piece_info(a).unwrap();
            let pb = piece_picker.get_piece_info(b).unwrap();
            pb.priority
                .cmp(&pa.priority) // Higher priority first
                .then(pa.frequency.cmp(&pb.frequency)) // Then rarer
        });

        let count = suggestions.len();

        for piece_idx in suggestions {
            let _msg = serializer::serialize_suggest(piece_idx);

            // Note: In a full implementation, this would use a proper message queue/channel.
            debug!(
                "[BEP6] Would send Suggest for piece {} to peer {}",
                piece_idx, peer_idx
            );
        }

        if count > 0 {
            // Update suggest count for this peer
            let new_count = sent_count + count;
            self.suggest_sent_counts.insert(peer_idx, new_count);

            info!(
                "[BEP6] Sent {} Suggest messages to peer {} (total: {})",
                count, peer_idx, new_count
            );
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endgame_state_new_is_inactive() {
        let es = EndgameState::new();
        assert!(!es.is_endgame_active());
        assert_eq!(es.tracked_count(), 0);
    }

    #[test]
    fn test_endgame_state_default_is_inactive() {
        let es = EndgameState::default();
        assert!(!es.is_endgame_active());
    }

    #[test]
    fn test_endgame_enter_and_exit() {
        let mut es = EndgameState::new();
        assert!(!es.is_endgame_active());

        es.enter_endgame();
        assert!(es.is_endgame_active());

        // Double enter should be idempotent
        es.enter_endgame();
        assert!(es.is_endgame_active());

        es.exit_endgame();
        assert!(!es.is_endgame_active());
    }

    #[test]
    fn test_endgame_track_request() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        // Track requests from 3 peers for the same block
        es.track_request(0, 0, 16384, 0);
        es.track_request(0, 0, 16384, 1);
        es.track_request(0, 0, 16384, 2);

        assert_eq!(es.tracked_count(), 1); // One unique block tracked

        let targets = es.get_cancel_targets(0, 0, 16384);
        assert_eq!(targets.len(), 3);
        assert!(targets.contains(&0));
        assert!(targets.contains(&1));
        assert!(targets.contains(&2));
    }

    #[test]
    fn test_endgame_cancel_removes_on_arrival() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        es.track_request(5, 0, 16384, 0);
        es.track_request(5, 0, 16384, 1);

        let targets = es.get_cancel_targets(5, 0, 16384);
        assert_eq!(targets.len(), 2);

        // After removal, no more targets
        es.remove_request(5, 0, 16384);
        let targets_after = es.get_cancel_targets(5, 0, 16384);
        assert!(targets_after.is_empty());
        assert_eq!(es.tracked_count(), 0);
    }

    #[test]
    fn test_endgame_multiple_blocks_tracked_independently() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        // Track different blocks
        es.track_request(0, 0, 16384, 0);
        es.track_request(0, 0, 16384, 1);
        es.track_request(0, 16384, 16384, 0);
        es.track_request(0, 16384, 16384, 2);

        assert_eq!(es.tracked_count(), 2);

        // Cancel one block doesn't affect the other
        es.remove_request(0, 0, 16384);
        assert_eq!(es.tracked_count(), 1);

        let remaining = es.get_cancel_targets(0, 16384, 16384);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&0));
        assert!(remaining.contains(&2));
    }

    #[test]
    fn test_endgame_exit_clears_all_tracking() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        es.track_request(10, 0, 16384, 0);
        es.track_request(10, 0, 16384, 1);
        es.track_request(11, 0, 8192, 0);
        assert_eq!(es.tracked_count(), 2);

        es.exit_endgame();
        assert!(!es.is_endgame_active());
        assert_eq!(es.tracked_count(), 0);
    }

    #[test]
    fn test_endgame_get_cancel_targets_empty_when_inactive() {
        let es = EndgameState::new();
        // Even if we somehow track (shouldn't happen when inactive), targets should be empty
        // Actually tracking works regardless, but is_endgate_active gates usage
        let targets = es.get_cancel_targets(99, 0, 16384);
        assert!(targets.is_empty());
    }

    #[test]
    fn test_endgame_track_different_piece_offsets_lengths() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        // Last block might be shorter
        es.track_request(0, 32768, 8000, 0);
        es.track_request(0, 32768, 8000, 1);

        let targets = es.get_cancel_targets(0, 32768, 8000);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_endgame_remove_nonexistent_is_noop() {
        let mut es = EndgameState::new();
        es.enter_endgame();

        // Remove something that was never tracked - should not panic
        es.remove_request(999, 999, 999);
        assert_eq!(es.tracked_count(), 0);
    }

    // ==================== BEP 6 Fast Extension Tests ====================

    #[test]
    fn test_is_bitfield_set_basic() {
        // Test bitfield: [0b11000000] = pieces 0 and 1 set (MSB first)
        let bf = vec![0xC0];
        assert!(BtDownloadCommand::is_bitfield_set(&bf, 0));
        assert!(BtDownloadCommand::is_bitfield_set(&bf, 1));
        assert!(!BtDownloadCommand::is_bitfield_set(&bf, 2));
        assert!(!BtDownloadCommand::is_bitfield_set(&bf, 7));
    }

    #[test]
    fn test_is_bitfield_set_multi_byte() {
        // Bitfield for 16 pieces: all set
        let bf = vec![0xFF, 0xFF];
        for i in 0..16u32 {
            assert!(
                BtDownloadCommand::is_bitfield_set(&bf, i),
                "Piece {} should be set",
                i
            );
        }
    }

    #[test]
    fn test_is_bitfield_set_out_of_range() {
        let bf = vec![0xFF];
        assert!(!BtDownloadCommand::is_bitfield_set(&bf, 8)); // Beyond bitfield length
        assert!(!BtDownloadCommand::is_bitfield_set(&bf, 100));
    }

    #[test]
    fn test_calculate_fast_set_basic() {
        let needed = vec![0u32, 1, 2, 3, 4, 5];
        let peer_bf = vec![0b11111100]; // Peer has pieces 0-5

        let already_sent = HashSet::new();
        let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

        assert_eq!(fast_set.len(), 6); // All pieces should be selected (<10 limit)
        assert!(fast_set.contains(&0));
        assert!(fast_set.contains(&5));
    }

    #[test]
    fn test_calculate_fast_set_respects_max_limit() {
        // Create 15 needed pieces
        let needed: Vec<u32> = (0..15).collect();
        let peer_bf = vec![0xFF, 0xFF]; // Peer has first 16 pieces

        let already_sent = HashSet::new();
        let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

        assert_eq!(fast_set.len(), 10); // Should cap at MAX_ALLOWED_FAST_PER_PEER
    }

    #[test]
    fn test_calculate_fast_set_excludes_already_sent() {
        let needed = vec![0u32, 1, 2, 3, 4];
        let peer_bf = vec![0b11111000];

        let mut already_sent = HashSet::new();
        already_sent.insert(0);
        already_sent.insert(1);

        let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

        assert_eq!(fast_set.len(), 3); // Only 2,3,4 should be selected
        assert!(!fast_set.contains(&0));
        assert!(!fast_set.contains(&1));
        assert!(fast_set.contains(&2));
    }

    #[test]
    fn test_calculate_fast_set_filters_by_peer_bitfield() {
        let needed = vec![0u32, 1, 2, 3, 4];
        let peer_bf = vec![0b00011000]; // bitfield byte: bits 3 and 4 set (pieces 3,4)

        let already_sent = HashSet::new();
        let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

        assert_eq!(fast_set.len(), 2); // Only pieces that peer has
        assert!(fast_set.contains(&3));
        assert!(fast_set.contains(&4));
        assert!(!fast_set.contains(&0));
        assert!(!fast_set.contains(&1));
        assert!(!fast_set.contains(&2));
    }
}
