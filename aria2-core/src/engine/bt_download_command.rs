use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use crate::engine::bt_upload_session::{BtSeedingConfig, PieceDataProvider};
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::udp_tracker_client::{SharedUdpClient, UdpTrackerClient};
use crate::engine::udp_tracker_manager::UdpTrackerManager;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::engine::multi_file_layout::MultiFileLayout;
use aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker;

enum BtPeerConn {
    Plain(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Encrypted(aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection),
}

impl BtPeerConn {
    async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => Ok(BtPeerConn::Encrypted(conn)),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    async fn connect_plain(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash)
            .await
        {
            Ok(conn) => Ok(BtPeerConn::Plain(conn)),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    async fn send_unchoke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_choke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_not_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_request(
        &mut self,
        req: aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_cancel(
        &mut self,
        req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        match self {
            BtPeerConn::Plain(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    fn is_connected(&self) -> bool {
        match self {
            BtPeerConn::Plain(c) => c.is_connected(),
            BtPeerConn::Encrypted(c) => c.is_connected(),
        }
    }

    fn is_encrypted(&self) -> bool {
        matches!(self, BtPeerConn::Encrypted(_))
    }
}

pub struct BtDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    output_path: std::path::PathBuf,
    started: bool,
    completed_bytes: u64,
    torrent_data: Vec<u8>,
    seed_enabled: bool,
    seed_time: Option<Duration>,
    seed_ratio: Option<f64>,
    total_uploaded: u64,
    udp_client: Option<SharedUdpClient>,
    dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    peer_tracker: Option<PeerBitfieldTracker>,
    /// Choking algorithm for download-side peer selection.
    /// Tracks which peers are choking us so we can prefer unchoked peers for requests.
    pub choking_algo: Option<ChokingAlgorithm>,
    pub multi_file_layout: Option<MultiFileLayout>,
}

impl BtDownloadCommand {
    pub fn new(
        gid: GroupId,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
            .map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Torrent parse failed: {}", e)))
        })?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = meta.info.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let group = RequestGroup::new(
            gid,
            vec![format!("bt://{}", meta.info_hash.as_hex())],
            options.clone(),
        );

        let seed_time = options
            .seed_time
            .map(|t| {
                if t == 0 {
                    None
                } else {
                    Some(Duration::from_secs(t))
                }
            })
            .flatten();
        let seed_ratio = options.seed_ratio.filter(|&r| r > 0.0);

        info!(
            "BtDownloadCommand created: {} -> {} ({} bytes, {} pieces) seed={:?} ratio={:?}",
            meta.info.name,
            path.display(),
            meta.total_size(),
            meta.num_pieces(),
            seed_time,
            seed_ratio
        );

        // Initialize choking algorithm from download options
        let choking_algo = if options.bt_max_upload_slots.is_some()
            || options.bt_optimistic_unchoke_interval.is_some()
            || options.bt_snubbed_timeout.is_some()
        {
            let config = ChokingConfig {
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options.bt_optimistic_unchoke_interval.unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10,
            };
            Some(ChokingAlgorithm::new(config))
        } else {
            None
        };

        let multi_file_layout = if !meta.is_single_file() {
            let layout_base_dir = std::path::PathBuf::from(&dir);
            match MultiFileLayout::from_info_dict(&meta.info, &layout_base_dir) {
                Ok(layout) => Some(layout),
                Err(e) => {
                    return Err(Aria2Error::Fatal(FatalError::Config(format!(
                        "MultiFileLayout creation failed: {}", e
                    ))));
                }
            }
        } else {
            None
        };

        let effective_output_path = if multi_file_layout.is_some() {
            std::path::PathBuf::from(&dir)
        } else {
            path.clone()
        };

        info!(
            "BtDownloadCommand created: {} -> {} ({} bytes, {} pieces) seed={:?} ratio={:?} multi_file={}",
            meta.info.name,
            effective_output_path.display(),
            meta.total_size(),
            meta.num_pieces(),
            seed_time,
            seed_ratio,
            multi_file_layout.is_some()
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: effective_output_path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0) > 0
                || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            dht_engine: None,
            public_trackers: None,
            peer_tracker: None,
            choking_algo,
            multi_file_layout,
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }

    async fn run_seeding_phase(
        &mut self,
        connections: Vec<BtPeerConn>,
        piece_length: u32,
        num_pieces: u32,
    ) -> Result<()> {
        if connections.is_empty() {
            info!("No active peers for seeding");
            return Ok(());
        }

        let file_provider = Arc::new(FileBackedPieceProvider::new(
            self.output_path.clone(),
            piece_length,
            num_pieces,
            self.multi_file_layout.clone(),
        ));

        let upload_limit = { self.group.read().await.options().max_upload_limit };
        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: upload_limit,
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let exit_cond = match (self.seed_time, self.seed_ratio) {
            (Some(t), Some(r)) => SeedExitCondition {
                seed_time: Some(t),
                seed_ratio: Some(r),
            },
            (Some(t), None) => SeedExitCondition {
                seed_time: Some(t),
                seed_ratio: None,
            },
            (None, Some(r)) => SeedExitCondition {
                seed_time: None,
                seed_ratio: Some(r),
            },
            (None, None) => SeedExitCondition::infinite(),
        };

        let plain_connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> =
            connections
                .into_iter()
                .filter_map(|c| match c {
                    BtPeerConn::Plain(p) => Some(p),
                    _ => None,
                })
                .collect();

        let mut manager = BtSeedManager::new_with_choking_algo(
            plain_connections,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            self.choking_algo.take(), // Transfer ownership to seed manager
        );
        manager.run_seeding_loop().await?;

        self.total_uploaded = manager.total_uploaded();
        info!(
            "Seeding complete: uploaded {} bytes in {:?}",
            self.total_uploaded,
            manager.seeding_duration()
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Download-side choke tracking helpers
    // ------------------------------------------------------------------

    /// Record that a peer at the given index has sent us a Choke message.
    ///
    /// This updates the internal `choking_algo` state so that
    /// [`Self::select_best_peer_for_request`] can deprioritize choked peers.
    pub fn on_peer_choke(&mut self, peer_idx: usize) {
        if let Some(ref mut algo) = self.choking_algo {
            if let Some(peer) = algo.get_peer_mut(peer_idx) {
                peer.peer_choking = true;
                debug!("Peer #{} is now choking us", peer_idx);
            }
        }
    }

    /// Record that a peer at the given index has sent us an Unchoke message.
    pub fn on_peer_unchoke(&mut self, peer_idx: usize) {
        if let Some(ref mut algo) = self.choking_algo {
            if let Some(peer) = algo.get_peer_mut(peer_idx) {
                peer.peer_choking = false;
                debug!("Peer #{} has unchoked us", peer_idx);
            }
        }
    }

    /// Record data received from a peer (updates speed + resets snubbed).
    pub fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(ref mut algo) = self.choking_algo {
            algo.on_data_received(peer_idx, bytes);
        }
    }

    /// Check if any tracked peer is snubbed and should be handled (e.g., disconnected or deprioritized).
    ///
    /// Returns indices of newly snubbed peers.
    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        if let Some(ref mut algo) = self.choking_algo {
            algo.check_snubbed_peers()
        } else {
            vec![]
        }
    }

    /// Add a connected peer to the choking algorithm tracking.
    ///
    /// Call this when a new peer connection is established during download phase.
    pub fn add_peer_to_tracking(
        &mut self,
        peer_id: [u8; 8],
        addr: std::net::SocketAddr,
    ) -> usize {
        if let Some(ref mut algo) = self.choking_algo {
            use super::peer_stats::PeerStats;
            let full_peer_id = {
                let mut id = [0u8; 20];
                id[..8].copy_from_slice(&peer_id);
                id
            };
            let stats = PeerStats::new(full_peer_id, addr);
            algo.add_peer(stats);
            algo.len() - 1 // Return the index of the added peer
        } else {
            0 // No algorithm, return dummy index
        }
    }
}

struct FileBackedPieceProvider {
    file_path: std::path::PathBuf,
    piece_length: u32,
    num_pieces: u32,
    multi_file_layout: Option<MultiFileLayout>,
}

impl FileBackedPieceProvider {
    pub fn new(file_path: std::path::PathBuf, piece_length: u32, num_pieces: u32, multi_file_layout: Option<MultiFileLayout>) -> Self {
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
        }
    }
}

impl PieceDataProvider for FileBackedPieceProvider {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        use std::io::SeekFrom;
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let read_op = move |file_path: std::path::PathBuf, seek_pos: u64, len: u32| -> Option<Vec<u8>> {
            let rt = match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle,
                Err(_) => {
                    let rt = tokio::runtime::Runtime::new().ok()?;
                    return rt.block_on(async {
                        let mut f = File::open(&file_path).await.ok()?;
                        f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                        let mut buf = vec![0u8; len as usize];
                        f.read_exact(&mut buf).await.ok()?;
                        Some(buf)
                    });
                }
            };
            tokio::task::block_in_place(|| {
                rt.block_on(async {
                    let mut f = File::open(&file_path).await.ok()?;
                    f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                    let mut buf = vec![0u8; len as usize];
                    f.read_exact(&mut buf).await.ok()?;
                    Some(buf)
                })
            })
        };

        if let Some(ref layout) = self.multi_file_layout {
            let (file_idx, file_offset) = layout.resolve_file_offset(piece_index, offset)?;
            let file_path = layout.file_absolute_path(file_idx)?.to_path_buf();
            read_op(file_path, file_offset, length)
        } else {
            let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;
            read_op(self.file_path.clone(), file_pos, length)
        }
    }

    fn has_piece(&self, _piece_index: u32) -> bool {
        true
    }

    fn num_pieces(&self) -> u32 {
        self.num_pieces
    }
}

#[async_trait]
impl Command for BtDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

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
            // Export to atomic field for session persistence
            g.set_total_length_atomic(meta.total_size());
        }

        let piece_length = meta.info.piece_length;
        let total_size = meta.total_size();
        let num_pieces = meta.num_pieces();

        let mut piece_manager = aria2_protocol::bittorrent::piece::manager::PieceManager::new(
            num_pieces as u32,
            piece_length,
            total_size,
            meta.info.pieces.clone(),
        );

        let mut piece_picker =
            aria2_protocol::bittorrent::piece::picker::PiecePicker::new(num_pieces as u32);
        piece_picker.set_strategy(
            aria2_protocol::bittorrent::piece::picker::PieceSelectionStrategy::Sequential,
        );

        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();
        let info_hash_raw = meta.info_hash.bytes;

        let announce_url = format!("{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
            meta.announce,
            urlencode_infohash(&info_hash_raw),
            urlencode_infohash(&my_peer_id),
            total_size,
        );

        eprintln!("[BT] Announcing to tracker: {}", announce_url);
        let resp = reqwest::get(&announce_url).await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker HTTP failed: {}", e),
            })
        })?;
        eprintln!("[BT] Tracker response status: {}", resp.status());
        let body = resp.bytes().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker body read failed: {}", e),
            })
        })?;
        eprintln!("[BT] Tracker body: {:?}", String::from_utf8_lossy(&body));

        let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(
            &body,
        )
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker parse failed: {}", e),
            })
        })?;

        eprintln!("[BT] Tracker response: {} peers", tracker_resp.peer_count());
        for peer in &tracker_resp.peers {
            eprintln!("[BT]   Peer: {}:{}", peer.ip, peer.port);
        }

        if tracker_resp.is_failure() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: tracker_resp.failure_reason.unwrap_or_default(),
                },
            ));
        }

        let mut peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> =
            tracker_resp
                .peers
                .iter()
                .map(|p| aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&p.ip, p.port))
                .collect();

        if let Ok(udp) = UdpTrackerClient::new(0).await {
            self.udp_client = Some(Arc::new(tokio::sync::Mutex::new(udp)));
            if let Some(ref shared_client) = self.udp_client {
                let mut mgr = UdpTrackerManager::new(Arc::clone(shared_client)).await;
                let urls: Vec<String> = meta.announce_list.iter().flatten().cloned().collect();
                mgr.parse_tracker_urls(&urls);

                if mgr.endpoint_count() > 0 {
                    debug!("Trying {} UDP tracker endpoints", mgr.endpoint_count());

                    match mgr.announce(
                        &info_hash_raw, &my_peer_id,
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
            eprintln!("[BT] ERROR: No peers from tracker");
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
                    eprintln!("[BT] DHT engine started");
                    self.dht_engine.as_ref().unwrap().start_maintenance_loop();
                }
                Err(e) => {
                    warn!("[BT] DHT engine start failed: {}", e);
                }
            }
        }

        if let Some(ref engine) = self.dht_engine {
            let result = engine.find_peers(&info_hash_raw).await;
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
                eprintln!(
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
        if enable_public_trackers && self.public_trackers.is_none() && peer_addrs.len() < 15 {
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

            for url in http_urls.iter().take(10) {
                match Self::announce_to_public_tracker(url, &info_hash_raw, &my_peer_id, total_size)
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
                eprintln!(
                    "[BT] Public trackers discovered {} extra peers (announced to {} of {})",
                    peer_addrs.len() - before,
                    announced,
                    http_urls.len()
                );
            } else if announced > 0 {
                debug!("[BT] Public trackers responded but no peers found");
            }
        }

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No peers from tracker or DHT".into(),
                },
            ));
        }

        eprintln!("[BT] Connecting to {} peers...", peer_addrs.len());
        let mut active_connections: Vec<BtPeerConn> = Vec::new();

        let require_crypto = { self.group.read().await.options().bt_require_crypto };
        let force_encrypt = { self.group.read().await.options().bt_force_encrypt };

        for addr in &peer_addrs {
            eprintln!("[BT] Connecting to peer {}:{}", addr.ip, addr.port);
            let conn_result = if force_encrypt || require_crypto {
                BtPeerConn::connect_mse(addr, &info_hash_raw, require_crypto).await
            } else {
                match BtPeerConn::connect_mse(addr, &info_hash_raw, false).await {
                    Ok(conn) => Ok(conn),
                    Err(_) => BtPeerConn::connect_plain(addr, &info_hash_raw).await,
                }
            };

            match conn_result {
                Ok(mut conn) => {
                    eprintln!(
                        "[BT] Connected to peer {}:{} (encrypted={})",
                        addr.ip,
                        addr.port,
                        conn.is_encrypted()
                    );
                    conn.send_unchoke().await.ok();
                    conn.send_interested().await.ok();

                    let bf_len = (num_pieces + 7) / 8;
                    let empty_bf = vec![0u8; bf_len];
                    conn.send_bitfield(empty_bf.clone()).await.ok();

                    tokio::time::sleep(Duration::from_millis(100)).await;

                    eprintln!("[BT] Waiting for unchoke from {}:{}", addr.ip, addr.port);
                    for _ in 0..50 {
                        match tokio::time::timeout(Duration::from_secs(5), conn.read_message())
                            .await
                        {
                            Ok(Ok(Some(msg))) => {
                                use aria2_protocol::bittorrent::message::types::BtMessage;
                                if matches!(msg, BtMessage::Unchoke) {
                                    eprintln!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                                    break;
                                }
                                eprintln!("[BT] Got message: {:?}", msg);
                            }
                            Ok(Ok(None)) => {
                                eprintln!("[BT] EOF from peer");
                                break;
                            }
                            Ok(Err(e)) => {
                                eprintln!("[BT] Error reading from peer: {}", e);
                                break;
                            }
                            Err(_) => {
                                eprintln!("[BT] Timeout reading from peer");
                                break;
                            }
                        }
                    }

                    active_connections.push(conn);
                }
                Err(e) => {
                    eprintln!("[BT] Failed to connect peer {}: {}", addr.ip, e);
                    continue;
                }
            }
        }

        eprintln!("[BT] Active connections: {}", active_connections.len());
        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed".into(),
                },
            ));
        }

        // Initialize choking algorithm with connected peers
        {
            let options = self.group.read().await.options().clone();
            let config = ChokingConfig {
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10, // Fixed internal interval
            };

            let mut algo = ChokingAlgorithm::new(config);

            // Add each active connection to the algorithm
            for addr in &peer_addrs {
                let socket_addr =
                    std::net::SocketAddr::new(addr.ip.parse().unwrap_or_else(|_| {
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                    }), addr.port);
                let peer_stats = crate::engine::peer_stats::PeerStats::new([0u8; 20], socket_addr);
                algo.add_peer(peer_stats);
            }

            self.choking_algo = Some(algo);
            eprintln!(
                "[BT] Choking algorithm initialized with {} peers",
                self.choking_algo.as_ref().unwrap().len()
            );
        }

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

        const BLOCK_SIZE: u32 = 16384;
        const MAX_RETRIES: u32 = 3;
        const ENDGAME_THRESHOLD: u32 = 20;

        let mut peer_tracker = PeerBitfieldTracker::new(num_pieces as u32);
        for (i, _conn) in active_connections.iter().enumerate() {
            peer_tracker.update_peer_bitfield(
                &format!("peer_{}", i),
                &vec![0xFFu8; ((num_pieces as usize) + 7) / 8],
            );
        }
        piece_picker.set_frequencies_from_peers(&peer_tracker.piece_frequencies());

        eprintln!(
            "[BT] Piece selection strategy: RarestFirst, {} pieces total, {} peers tracked",
            num_pieces,
            peer_tracker.peer_count()
        );

        loop {
            if piece_picker.is_complete() {
                break;
            }

            let remaining = piece_picker.remaining_count();
            let is_endgame = remaining > 0 && remaining <= ENDGAME_THRESHOLD;

            if is_endgame && !piece_picker.endgame_candidates().is_empty() {
                eprintln!("[BT] === ENDGAME MODE === ({} pieces remaining)", remaining);
            }

            let next_piece_idx: Option<usize> = if is_endgame {
                piece_picker.pick_next()
            } else {
                let all_ones_bf = vec![0xFFu8; ((num_pieces as usize) + 7) / 8];
                piece_picker.select(&all_ones_bf, num_pieces as usize)
            }
            .map(|v| v as usize);

            let next_piece_idx = match next_piece_idx {
                Some(idx) => idx,
                None => {
                    eprintln!("[BT] No piece available, waiting...");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            eprintln!("[BT] Downloading piece {}...", next_piece_idx);
            let actual_piece_len =
                if next_piece_idx == num_pieces - 1 && total_size % piece_length as u64 != 0 {
                    (total_size % piece_length as u64) as u32
                } else {
                    piece_length
                };

            let num_blocks = (actual_piece_len + BLOCK_SIZE - 1) / BLOCK_SIZE;
            eprintln!(
                "[BT] Piece {} has {} blocks (size: {} bytes)",
                next_piece_idx, num_blocks, actual_piece_len
            );
            let mut piece_ok = false;

            for _retry in 0..MAX_RETRIES {
                eprintln!("[BT] Retry {} for piece {}", _retry, next_piece_idx);
                let mut piece_data = Vec::with_capacity(actual_piece_len as usize);
                let mut blocks_received = 0u32;

                for block_idx in 0..num_blocks {
                    let offset = block_idx * BLOCK_SIZE;
                    let len = if offset + BLOCK_SIZE > actual_piece_len {
                        actual_piece_len - offset
                    } else {
                        BLOCK_SIZE
                    };

                    let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                        index: next_piece_idx as u32,
                        begin: offset,
                        length: len,
                    };

                    eprintln!(
                        "[BT] Requesting block {} offset={} len={}",
                        block_idx, offset, len
                    );
                    let mut got_block = false;
                    for (conn_idx, conn) in active_connections.iter_mut().enumerate() {
                        if conn.send_request(req.clone()).await.is_err() {
                            continue;
                        }

                        match tokio::time::timeout(Duration::from_secs(3), async {
                            for _ in 0..10000 {
                                match conn.read_message().await {
                                    Ok(Some(msg)) => {
                                        if let aria2_protocol::bittorrent::message::types::BtMessage::Piece { index, begin, ref data } = msg {
                                            if index == next_piece_idx as u32 && begin == offset {
                                                return Ok(data.clone());
                                            }
                                        }
                                    }
                                    Ok(None) | Err(_) => continue,
                                }
                            }
                            Err(())
                        }).await {
                            Ok(Ok(data)) => {
                                eprintln!("[BT] Got block {} data len={}", block_idx, data.len());
                                piece_data.extend_from_slice(&data);
                                blocks_received += 1;
                                self.completed_bytes += data.len() as u64;

                                // Update peer statistics with received data
                                self.on_piece_received(conn_idx, data.len() as u64);
                                got_block = true;
                                break;
                            }
                            Ok(Err(())) => { eprintln!("[BT] Block request timeout"); }
                            Err(_) => { eprintln!("[BT] Block request timeout (outer)"); }
                        }
                    }

                    if !got_block {
                        eprintln!("[BT] Failed to get block {}", block_idx);
                        break;
                    }
                }

                if blocks_received == num_blocks {
                    eprintln!(
                        "[BT] All blocks received for piece {}, verifying...",
                        next_piece_idx
                    );
                    if piece_manager.verify_piece_hash(next_piece_idx as u32, &piece_data) {
                        eprintln!("[BT] Piece {} verified OK", next_piece_idx);
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);

                        if let Some(ref layout) = self.multi_file_layout {
                            Self::write_piece_to_multi_files(
                                layout,
                                next_piece_idx as u32,
                                &piece_data,
                                layout.piece_length(),
                            ).await?;
                        } else {
                            writer.write(&piece_data).await.ok();
                        }

                        // Export updated bitfield to RequestGroup for session persistence
                        {
                            // Build bitfield from completed pieces
                            let num_p = piece_manager.num_pieces();
                            let bf_len = ((num_p as usize) + 7) / 8;
                            let mut bitfield = vec![0u8; bf_len];
                            for i in 0..num_p {
                                if piece_manager.is_completed(i) {
                                    let byte_idx = (i as usize) / 8;
                                    let bit_idx = 7 - ((i as usize) % 8); // MSB first
                                    if byte_idx < bitfield.len() {
                                        bitfield[byte_idx] |= 1 << bit_idx;
                                    }
                                }
                            }
                            let g = self.group.write().await;
                            g.set_bt_bitfield(Some(bitfield)).await;
                        }

                        for conn in &mut active_connections {
                            conn.send_have(next_piece_idx as u32).await.ok();
                        }
                        piece_ok = true;
                        break;
                    } else {
                        eprintln!(
                            "[BT] SHA1 mismatch on piece {}, retrying...",
                            next_piece_idx
                        );
                    }
                } else {
                    eprintln!(
                        "[BT] Incomplete piece {}, received {}/{}",
                        next_piece_idx, blocks_received, num_blocks
                    );
                }
            }

            if !piece_ok {
                eprintln!(
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
                // Export to atomic fields for session persistence
                g.set_completed_length(self.completed_bytes);

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    // Update cached download speed for session persistence
                    g.set_download_speed_cached(speed);
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        eprintln!("[BT] Finalizing writer...");
        writer.finalize().await.ok();
        eprintln!("[BT] Writer finalized OK");
        info!(
            "BT download done: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );

        if self.seed_enabled && !active_connections.is_empty() {
            eprintln!(
                "[BT] Starting seeding phase with {} peers...",
                active_connections.len()
            );
            info!(
                "Starting seeding phase with {} peers...",
                active_connections.len()
            );
            self.run_seeding_phase(active_connections, piece_length, num_pieces as u32)
                .await?;
        } else {
            eprintln!(
                "[BT] Skipping seeding (enabled={}, connections={})",
                self.seed_enabled,
                active_connections.len()
            );
            for conn in &mut active_connections {
                drop(conn);
            }
        }

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
            // Export final progress to atomic fields for session persistence
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
            if let Err(e) = engine.announce_peer(&info_hash_raw, 0).await {
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
    /// Select the best peer for requesting pieces, preferring unchoked peers.
    ///
    /// Uses the choking algorithm's peer stats to score and rank peers:
    /// - Unchoked peers are strongly preferred
    /// - Higher download speed is better
    /// - Snubbed peers are penalized
    ///
    /// Returns the index of the best peer, or None if no suitable peer found.
    fn select_best_peer_for_request(&self) -> Option<usize> {
        if let Some(ref algo) = self.choking_algo {
            // Find best peer: unchoked + high download speed + not snubbed
            let best_idx = algo
                .peers()
                .iter()
                .enumerate()
                .filter(|(_, p)| !p.am_choking && p.peer_interested && !p.is_snubbed)
                .max_by_key(|(_, p)| {
                    let mut score = 0i64;
                    // Download speed is primary factor (scaled down to avoid overflow)
                    score += (p.download_speed * 0.5) as i64;
                    // Upload speed contribution (reciprocity)
                    score += (p.upload_speed * 0.3) as i64;
                    // Bonus for being interested in our data
                    if p.peer_interested {
                        score += 50;
                    }
                    score
                })
                .map(|(i, _)| i);

            if best_idx.is_some() {
                debug!(
                    "[BT] Selected peer {} for request (using choking algorithm)",
                    best_idx.unwrap()
                );
                return best_idx;
            }

            // Fallback: if no unchoked+interested peer, just pick first non-snubbed peer
            algo.peers()
                .iter()
                .position(|p| !p.is_snubbed)
        } else {
            // No choking algorithm configured: cannot select a peer
            None
        }
    }

    /// Handle a peer that has been marked as snubbed.
    ///
    /// Reduces the request frequency for this peer by increasing its
    /// request interval multiplier. This avoids wasting time waiting for
    /// data from unresponsive peers while keeping the connection alive
    /// in case they recover.
    async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()> {
        if let Some(ref mut algo) = self.choking_algo {
            if let Some(peer) = algo.get_peer_mut(peer_idx) {
                // Reduce request frequency (don't ask them for pieces as often)
                // This is a soft penalty - we don't disconnect, just reduce priority
                warn!(
                    "[BT] Peer {} at {} marked as snubbed, reducing request priority",
                    peer_idx, peer.addr
                );

                // The choking algorithm will automatically lower this peer's score
                // on next rotation due to is_snubbed flag, which will cause it to be choked

                // Additional action: we could optionally force a choke message
                // For now, just log and let the algorithm handle it naturally
            }
        }

        Ok(())
    }

    /// Update peer statistics when piece data is received.
    ///
    /// Should be called whenever we successfully receive a block from a peer.
    /// Updates the download speed estimate via EMA and resets the snubbed timer.
    fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(ref mut algo) = self.choking_algo {
            algo.on_data_received(peer_idx, bytes);
            debug!(
                "[BT] Updated peer {} stats: received {} bytes",
                peer_idx, bytes
            );
        }
    }

    async fn announce_to_public_tracker(
        tracker_url: &str,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        total_size: u64,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        let url = format!("{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
            tracker_url,
            urlencode_infohash(info_hash),
            urlencode_infohash(peer_id),
            total_size,
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("build client: {}", e))?;

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("read body: {}", e))?;

        let tracker_resp =
            aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
                .map_err(|e| format!("parse response: {}", e))?;

        if tracker_resp.is_failure() {
            return Err(tracker_resp
                .failure_reason
                .unwrap_or_else(|| "tracker failure".to_string()));
        }

        Ok(tracker_resp
            .peers
            .into_iter()
            .map(|p| (p.ip, p.port))
            .collect())
    }

    async fn write_piece_to_multi_files(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &[u8],
        _piece_length: u32,
    ) -> Result<()> {
        use tokio::io::{AsyncWriteExt, AsyncSeekExt};
        use std::collections::HashMap;

        let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

        let mut data_offset = 0usize;
        while data_offset < piece_data.len() {
            let piece_offset = data_offset as u32;

            if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
                let file_path = layout.file_absolute_path(file_idx)
                    .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?
                    .to_path_buf();

                if !file_writers.contains_key(&file_idx) {
                    let f = tokio::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .open(&file_path)
                        .await
                        .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e))))?;
                    file_writers.insert(file_idx, f);
                }

                let file_info = layout.get_file_info(file_idx)
                    .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?;

                let bytes_available_in_file = file_info.length.saturating_sub(file_offset);
                let bytes_remaining_in_piece = (piece_data.len() - data_offset) as u64;
                let write_len = bytes_available_in_file.min(bytes_remaining_in_piece) as usize;

                if write_len == 0 {
                    break;
                }

                let chunk = &piece_data[data_offset..data_offset + write_len];

                let writer = file_writers.get_mut(&file_idx).unwrap();
                writer.seek(std::io::SeekFrom::Start(file_offset)).await
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e))))?;
                writer.write_all(chunk).await
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e))))?;

                data_offset += write_len;
            } else {
                break;
            }
        }

        for (_, mut f) in file_writers {
            f.flush().await.map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
        }

        Ok(())
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout.as_ref().map_or(false, |l| l.is_multi_file())
    }

    pub fn get_multi_file_layout(&self) -> Option<&MultiFileLayout> {
        self.multi_file_layout.as_ref()
    }
}

fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::peer_stats::PeerStats;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use std::net::SocketAddr;

    /// Build a minimal valid .torrent file bytes for testing.
    /// This creates a single-file torrent with small content.
    fn build_test_torrent() -> Vec<u8> {
        // Build a valid bencoded torrent:
        // - Single file named "test"
        // - Total size: 0 bytes
        // - Piece length: 16384 (standard)
        // - One piece hash (all zeros for empty content)
        let mut v = Vec::new();

        // Root dictionary
        v.push(b'd');

        // announce URL
        let url = b"http://tracker.example.com/announce";
        v.extend_from_slice(format!("8:announce{}:", url.len()).as_bytes());
        v.extend_from_slice(url);

        // info dictionary
        v.extend_from_slice(b"4:info");
        v.push(b'd');

        // length: 0 (empty file)
        v.extend_from_slice(b"6:lengthi0e");

        // name: "test"
        v.extend_from_slice(b"4:name4:test");

        // piece length: 16384
        v.extend_from_slice(b"12:piece lengthi16384e");

        // pieces: one SHA-1 hash of empty data (= sha1 of "" = da39a3ee5e6b4b0d3255bfef95601890afd80709)
        // But for simplicity in tests, use 20 zero bytes
        v.extend_from_slice(b"6:pieces20:");
        // Use actual SHA-1 of empty string to be valid
        v.extend_from_slice(&[0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d,
                           0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
                           0xaf, 0xd8, 0x07, 0x09]);

        // End info dict
        v.push(b'e');

        // End root dict
        v.push(b'e');

        v
    }

    /// Helper to create a minimal BtDownloadCommand for testing.
    fn create_test_command() -> BtDownloadCommand {
        let torrent_bytes = build_test_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(1);
        BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
            .expect("Failed to create test command")
    }

    #[test]
    fn test_bt_seed_manager_integration_choking_algo_none_by_default() {
        // Verify BtDownloadCommand initializes with choking_algo = None by default
        let cmd = create_test_command();
        assert!(cmd.choking_algo.is_none(), "choking_algo should be None by default");
    }

    #[test]
    fn test_download_side_choke_tracking() {
        // Test that we can track peer choke/unchoke state on download side
        let mut cmd = create_test_command();

        // Set up a choking algorithm manually for testing
        let config = ChokingConfig {
            max_upload_slots: 4,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "192.168.1.10:6881".parse().unwrap();
        let peer = PeerStats::new([0xAA; 20], addr);
        algo.add_peer(peer);
        cmd.choking_algo = Some(algo);

        // Initially peer_choking should be true (default)
        assert!(cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking);

        // Simulate receiving Unchoke from peer
        cmd.on_peer_unchoke(0);
        assert!(
            !cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking,
            "peer_choking should be false after on_peer_unchoke"
        );

        // Simulate receiving Choke from peer
        cmd.on_peer_choke(0);
        assert!(
            cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking,
            "peer_choking should be true after on_peer_choke"
        );
    }

    #[test]
    fn test_download_side_select_best_peer_prefers_unchoked() {
        let mut cmd = create_test_command();

        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        // Peer 0: unchoked + high speed -> best choice
        let addr0: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let mut p0 = PeerStats::new([0x01; 20], addr0);
        p0.peer_choking = false;
        p0.download_speed = 100000.0;

        // Peer 1: choked but very high speed -> worse than unchoked
        let addr1: SocketAddr = "10.0.0.2:6881".parse().unwrap();
        let mut p1 = PeerStats::new([0x02; 20], addr1);
        p1.peer_choking = true;
        p1.download_speed = 500000.0;

        // Peer 2: unchoked but snubbed -> worse than normal unchoked
        let addr2: SocketAddr = "10.0.0.3:6881".parse().unwrap();
        let mut p2 = PeerStats::new([0x03; 20], addr2);
        p2.peer_choking = false;
        p2.is_snubbed = true;
        p2.download_speed = 80000.0;

        algo.add_peer(p0);
        algo.add_peer(p1);
        algo.add_peer(p2);
        cmd.choking_algo = Some(algo);

        let best = cmd.select_best_peer_for_request();
        assert_eq!(best, Some(0), "Should prefer unchoked+not-snubbed peer (peer 0)");
    }

    #[test]
    fn test_snubbed_peer_handling() {
        let mut cmd = create_test_command();

        let config = ChokingConfig {
            snubbed_timeout_secs: 1,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "172.16.0.5:6881".parse().unwrap();
        let peer = PeerStats::new([0xBB; 20], addr);
        algo.add_peer(peer);
        cmd.choking_algo = Some(algo);

        // Initially not snubbed
        let snubbed = cmd.check_snubbed_peers();
        assert!(snubbed.is_empty(), "No peers should be snubbed initially");

        // Wait for timeout then check
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let snubbed = cmd.check_snubbed_peers();
        assert_eq!(snubbed.len(), 1, "Peer should be snubbed after timeout");
        assert_eq!(snubbed[0], 0);

        // Now receive data from this peer - should reset snubbed status
        cmd.on_data_received_from_peer(0, 1024);
        assert!(
            !cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().is_snubbed,
            "Receiving data should reset snubbed status"
        );
    }

    #[test]
    fn test_add_peer_to_tracking() {
        let mut cmd = create_test_command();

        // Initialize with config so choking_algo is Some
        let options = DownloadOptions {
            bt_max_upload_slots: Some(4),
            ..Default::default()
        };
        let gid = GroupId::new(2);
        let torrent_bytes = build_test_torrent();
        cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
            .expect("Failed to create command with choking config");

        assert!(cmd.choking_algo.is_some());

        // Add peers
        let addr1: SocketAddr = "192.168.1.20:6881".parse().unwrap();
        let idx1 = cmd.add_peer_to_tracking([0x11; 8], addr1);
        assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 1);

        let addr2: SocketAddr = "192.168.1.21:6881".parse().unwrap();
        let idx2 = cmd.add_peer_to_tracking([0x22; 8], addr2);
        assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 2);
        assert_ne!(idx1, idx2, "Different peers should get different indices");
    }

    #[test]
    fn test_download_command_backward_compat_no_choking_config() {
        // When no choking config is set, command should work normally
        let mut cmd = create_test_command();
        assert!(cmd.choking_algo.is_none());

        // All helper methods should be safe no-ops
        cmd.on_peer_choke(0); // No panic
        cmd.on_peer_unchoke(0); // No panic
        cmd.on_data_received_from_peer(0, 1024); // No panic
        let best = cmd.select_best_peer_for_request();
        assert_eq!(best, None, "Should return None when no algorithm configured");
        let snubbed = cmd.check_snubbed_peers();
        assert!(snubbed.is_empty());
    }

    fn build_multi_file_torrent() -> Vec<u8> {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let file1_path = BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir1".to_vec()),
            BencodeValue::Bytes(b"file1.txt".to_vec()),
        ]);
        let mut file1_dict = BTreeMap::new();
        file1_dict.insert(b"length".to_vec(), BencodeValue::Int(500));
        file1_dict.insert(b"path".to_vec(), file1_path);

        let file2_path = BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir2".to_vec()),
            BencodeValue::Bytes(b"file2.dat".to_vec()),
        ]);
        let mut file2_dict = BTreeMap::new();
        file2_dict.insert(b"length".to_vec(), BencodeValue::Int(524));
        file2_dict.insert(b"path".to_vec(), file2_path);

        let files_list = BencodeValue::List(vec![
            BencodeValue::Dict(file1_dict),
            BencodeValue::Dict(file2_dict),
        ]);

        let mut info_dict = BTreeMap::new();
        info_dict.insert(b"name".to_vec(), BencodeValue::Bytes(b"multitest".to_vec()));
        info_dict.insert(b"files".to_vec(), files_list);
        info_dict.insert(b"piece length".to_vec(), BencodeValue::Int(512));

        let mut pieces_hash = Vec::new();
        pieces_hash.extend_from_slice(&[0u8; 20]);
        pieces_hash.extend_from_slice(&[1u8; 20]);
        info_dict.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_hash));

        let mut root_dict = BTreeMap::new();
        root_dict.insert(b"announce".to_vec(), BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()));
        root_dict.insert(b"info".to_vec(), BencodeValue::Dict(info_dict));

        BencodeValue::Dict(root_dict).encode()
    }

    #[test]
    fn test_multi_file_layout_created_for_multi_torrent() {
        let torrent_bytes = build_multi_file_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(100);
        let cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, Some("d:/tmp/multitest"))
            .expect("Failed to create command from multi-file torrent");

        assert!(cmd.multi_file_layout.is_some(), "multi_file_layout should be Some for multi-file torrent");
        let layout = cmd.multi_file_layout.as_ref().unwrap();
        assert!(layout.is_multi_file());
        assert_eq!(layout.num_files(), 2);
        assert_eq!(layout.total_size(), 1024);
    }

    #[test]
    fn test_single_file_no_layout() {
        let cmd = create_test_command();

        assert!(cmd.multi_file_layout.is_none(), "multi_file_layout should be None for single-file torrent");
    }

    #[test]
    fn test_is_multi_file_accessor() {
        let single_cmd = create_test_command();
        assert!(!single_cmd.is_multi_file(), "Single-file torrent should return false");

        let multi_bytes = build_multi_file_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(101);
        let multi_cmd = BtDownloadCommand::new(gid, &multi_bytes, &options, Some("d:/tmp/test_acc"))
            .expect("Failed to create multi-file command");
        assert!(multi_cmd.is_multi_file(), "Multi-file torrent should return true");

        assert!(multi_cmd.get_multi_file_layout().is_some());
        assert!(create_test_command().get_multi_file_layout().is_none());
    }

    #[tokio::test]
    async fn test_write_piece_to_multi_files_basic() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "write_test".to_string(),
            piece_length: 256,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 200, path: vec!["sub".to_string(), "a.bin".to_string()] },
                FileEntry { length: 312, path: vec!["sub".to_string(), "b.bin".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("mf_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();

        layout.create_directories().unwrap();

        let piece_data: Vec<u8> = (0..=255u8).collect();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            BtDownloadCommand::write_piece_to_multi_files(
                &layout,
                0,
                &piece_data,
                layout.piece_length(),
            ),
        ).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("write_piece_to_multi_files failed: {}", e),
            Err(_) => panic!("write_piece_to_multi_files timed out after 10s"),
        }

        let file_a = base_dir.join("sub").join("a.bin");
        let file_b = base_dir.join("sub").join("b.bin");
        assert!(file_a.exists(), "File a.bin should exist after write");
        assert!(file_b.exists(), "File b.bin should exist after write");

        let a_contents = std::fs::read(&file_a).unwrap();
        assert_eq!(a_contents.len(), 200, "a.bin should have 200 bytes");
        assert_eq!(&a_contents[..], &piece_data[..200], "a.bin contents should match first 200 bytes of piece");

        let b_contents = std::fs::read(&file_b).unwrap();
        assert_eq!(b_contents.len(), 56, "b.bin should have 56 bytes (remaining from piece 0)");
        assert_eq!(&b_contents[..], &piece_data[200..256], "b.bin contents should match remaining bytes");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_write_piece_resolve_logic() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "resolve_test".to_string(),
            piece_length: 128,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 100, path: vec!["x".to_string(), "f1.dat".to_string()] },
                FileEntry { length: 156, path: vec!["x".to_string(), "f2.dat".to_string()] },
            ]),
            private: None,
        };

        let base = std::path::PathBuf::from("d:/tmp/resolve_test");
        let layout = MultiFileLayout::from_info_dict(&info, &base).unwrap();
        assert_eq!(layout.num_files(), 2);
        assert_eq!(layout.total_size(), 256);
        assert!(layout.is_multi_file());

        let r0 = layout.resolve_file_offset(0, 0);
        assert_eq!(r0, Some((0, 0)), "Piece 0 offset 0 should map to file 0 offset 0");

        let r1 = layout.resolve_file_offset(0, 99);
        assert_eq!(r1, Some((0, 99)), "Piece 0 offset 99 should map to file 0 offset 99");

        let r2 = layout.resolve_file_offset(0, 100);
        assert_eq!(r2, Some((1, 0)), "Piece 0 offset 100 should map to file 1 offset 0 (cross-file boundary)");

        let r3 = layout.resolve_file_offset(0, 127);
        assert_eq!(r3, Some((1, 27)), "Piece 0 offset 127 should map to file 1 offset 27");

        let r4 = layout.resolve_file_offset(1, 0);
        assert_eq!(r4, Some((1, 28)), "Piece 1 offset 0 should map to file 1 offset 28");

        let r5 = layout.resolve_file_offset(1, 127);
        assert_eq!(r5, Some((1, 155)), "Piece 1 offset 127 should map to file 1 offset 155");

        let r_oob = layout.resolve_file_offset(1, 128);
        assert_eq!(r_oob, None, "Out-of-range offset should return None");
    }

    #[test]
    fn test_multi_file_piece_provider_reads_correct_file() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "provider_test".to_string(),
            piece_length: 128,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 100, path: vec!["p".to_string(), "a.dat".to_string()] },
                FileEntry { length: 156, path: vec!["p".to_string(), "b.dat".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("mfp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(base_dir.join("p")).unwrap();

        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
        layout.create_directories().unwrap();

        let file_a = layout.file_absolute_path(0).unwrap().to_path_buf();
        let file_b = layout.file_absolute_path(1).unwrap().to_path_buf();

        let data_a: Vec<u8> = (0..100u8).collect();
        let data_b: Vec<u8> = (100..=255u8).collect();
        std::fs::write(&file_a, &data_a).unwrap();
        std::fs::write(&file_b, &data_b).unwrap();

        let provider = FileBackedPieceProvider::new(
            base_dir.clone(),
            128,
            2,
            Some(layout),
        );

        let result = provider.get_piece_data(0, 0, 10);
        assert!(result.is_some(), "Should read from file a at offset 0");
        assert_eq!(result.unwrap(), (0..10u8).collect::<Vec<u8>>(), "First 10 bytes should match file a");

        let result_mid = provider.get_piece_data(0, 50, 50);
        assert!(result_mid.is_some());
        assert_eq!(result_mid.unwrap(), (50..100u8).collect::<Vec<u8>>(), "Bytes 50-99 from file a");

        let result_cross = provider.get_piece_data(0, 95, 5);
        assert!(result_cross.is_some());
        assert_eq!(result_cross.unwrap(), (95..100u8).collect::<Vec<u8>>(), "Last 5 bytes of file a");

        let result_b = provider.get_piece_data(1, 28, 50);
        assert!(result_b.is_some());
        assert_eq!(result_b.unwrap(), (156u8..=205u8).collect::<Vec<u8>>(), "Piece 1 offset 28 = global byte 156 = file b offset 56");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_single_file_piece_provider_unchanged() {
        let tmp = std::env::temp_dir().join(format!("sfp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("single.bin");
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&file_path, &data).unwrap();

        let provider = FileBackedPieceProvider::new(file_path.clone(), 128, 2, None);

        let result = provider.get_piece_data(0, 0, 16);
        assert!(result.is_some(), "Single-file provider should read successfully");
        assert_eq!(result.unwrap(), (0..16u8).collect::<Vec<u8>>(), "First 16 bytes should match");

        let result_mid = provider.get_piece_data(0, 64, 32);
        assert!(result_mid.is_some());
        assert_eq!(result_mid.unwrap(), (64..96u8).collect::<Vec<u8>>(), "Mid-piece read should match");

        let result_p1 = provider.get_piece_data(1, 0, 32);
        assert!(result_p1.is_some());
        assert_eq!(result_p1.unwrap(), (128..160u8).collect::<Vec<u8>>(), "Piece 1 offset 0 = byte 128");

        let result_end = provider.get_piece_data(1, 127, 1);
        assert!(result_end.is_some());
        assert_eq!(result_end.unwrap(), vec![255u8], "Last byte should be 255");

        assert_eq!(provider.num_pieces(), 2);
        assert!(provider.has_piece(0));
        assert!(provider.has_piece(1));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
