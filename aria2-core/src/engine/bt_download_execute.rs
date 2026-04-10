use async_trait::async_trait;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::engine::bt_download_command::{
    BtDownloadCommand, BLOCK_SIZE, MAX_RETRIES, PEER_CONNECTION_DELAY_MS, PUBLIC_TRACKER_PEER_THRESHOLD,
    MAX_PUBLIC_TRACKERS_TO_TRY,
};
use crate::engine::bt_message_handler::BtMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerInteraction;
use crate::engine::bt_piece_downloader::write_piece_to_multi_files;
use crate::engine::bt_piece_selector::{BtPieceSelector, build_bitfield_from_completed};
use crate::engine::bt_tracker_comm::{announce_to_public_tracker, perform_http_tracker_announce};
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::udp_tracker_client::UdpTrackerClient;
use crate::engine::udp_tracker_manager::UdpTrackerManager;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::engine::peer_stats::PeerStats;
use aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker;

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let (meta, piece_length, total_size, num_pieces) = self.prepare_environment().await?;

        let mut peer_addrs = self.discover_peers(&meta, total_size, &meta.info_hash.bytes).await?;

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No peers from tracker or DHT".into(),
                },
            ));
        }

        let mut active_connections = self.connect_to_peers(&peer_addrs, &meta.info_hash.bytes, num_pieces).await?;

        self.download_pieces_loop(
            &mut active_connections,
            &meta,
            piece_length,
            total_size,
            num_pieces,
        ).await?;

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
                drop(conn);
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
        if let Some(parent) = self.output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
                })?;
            }
        }

        if let Some(ref layout) = self.multi_file_layout {
            layout.create_directories().map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("create_directories failed: {}", e)))
            })?;
            info!("[BT] Multi-file mode: {} files under {}", layout.num_files(), self.output_path.display());
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
        if enable_public_trackers && self.public_trackers.is_none() && peer_addrs.len() < PUBLIC_TRACKER_PEER_THRESHOLD {
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
                match announce_to_public_tracker(url, info_hash_raw, &my_peer_id, total_size)
                    .await
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
        ).await?;

        let mut active_connections = conn_result.connections;

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
                let socket_addr =
                    std::net::SocketAddr::new(addr.ip.parse().unwrap_or_else(|_| {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                    }), addr.port);
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
        active_connections: &mut Vec<BtPeerConn>,
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

        let mut peer_tracker = PeerBitfieldTracker::new(num_pieces);
        BtPeerInteraction::initialize_peer_tracking(
            active_connections,
            num_pieces,
            &mut peer_tracker,
        );

        piece_selector.initialize_frequencies(&mut piece_picker, &peer_tracker);

        tracing::info!(
            "[BT] Piece selection strategy: RarestFirst, {} pieces total, {} peers tracked",
            num_pieces,
            peer_tracker.peer_count()
        );

        loop {
            if BtPieceSelector::is_complete(&piece_picker) {
                break;
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

            let actual_piece_len = piece_selector.calculate_piece_length(
                next_piece_idx,
                piece_length,
                total_size,
            );

            let num_blocks = BtPieceSelector::calculate_num_blocks(actual_piece_len, BLOCK_SIZE);
            tracing::debug!(
                "[BT] Piece {} has {} blocks (size: {} bytes)",
                next_piece_idx, num_blocks, actual_piece_len
            );
            let mut piece_ok = false;

            match BtMessageHandler::download_piece_blocks(
                active_connections,
                next_piece_idx as u32,
                actual_piece_len,
                num_blocks,
            ).await {
                Ok(piece_data) => {
                    self.completed_bytes += piece_data.len() as u64;

                    tracing::info!(
                        "[BT] All blocks received for piece {}, verifying...",
                        next_piece_idx
                    );
                    if piece_manager.verify_piece_hash(next_piece_idx as u32, &piece_data) {
                        tracing::info!("[BT] Piece {} verified OK", next_piece_idx);
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);

                        if let Some(ref layout) = self.multi_file_layout {
                            write_piece_to_multi_files(
                                layout,
                                next_piece_idx as u32,
                                &piece_data,
                                layout.piece_length(),
                            ).await?;
                        } else {
                            writer.write(&piece_data).await.ok();
                        }

                        {
                            let bitfield = build_bitfield_from_completed(
                                piece_manager.num_pieces(),
                                |i| piece_manager.is_completed(i),
                            );
                            let g = self.group.write().await;
                            g.set_bt_bitfield(Some(bitfield)).await;
                        }

                        BtPeerInteraction::broadcast_have(active_connections, next_piece_idx as u32).await;
                        piece_ok = true;
                    } else {
                        tracing::warn!(
                            "[BT] SHA1 mismatch on piece {}, retrying...",
                            next_piece_idx
                        );
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "[BT] Incomplete piece {}, needed {} blocks",
                        next_piece_idx, num_blocks
                    );
                }
            }

            if !piece_ok {
                tracing::error!(
                    "[BT] Piece {} failed after {} retries",
                    next_piece_idx, MAX_RETRIES
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

        Ok(())
    }
}
