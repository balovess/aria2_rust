use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use tracing::{info, debug, warn};

use crate::error::{Aria2Error, Result, RecoverableError, FatalError};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::bt_upload_session::{BtUploadSession, BtSeedingConfig, PieceDataProvider, InMemoryPieceProvider};
use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use crate::engine::udp_tracker_client::{UdpTrackerClient, SharedUdpClient};
use crate::engine::udp_tracker_manager::UdpTrackerManager;
use crate::request::request_group::{RequestGroup, GroupId, DownloadOptions};
use crate::filesystem::disk_writer::{DiskWriter, DefaultDiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
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
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash).await {
            Ok(conn) => Ok(BtPeerConn::Plain(conn)),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    async fn send_unchoke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_unchoke().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_unchoke().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_choke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_choke().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_choke().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_interested().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_interested().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_not_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_not_interested().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_not_interested().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_have(piece_index).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_have(piece_index).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_request(&mut self, req: aria2_protocol::bittorrent::message::types::PieceBlockRequest) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_request(req).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_request(req).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_cancel(&mut self, req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_cancel(req).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_cancel(req).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_bitfield(bitfield).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.send_bitfield(bitfield).await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
        }
    }

    async fn read_message(&mut self) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        match self {
            BtPeerConn::Plain(c) => c.read_message().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
            BtPeerConn::Encrypted(c) => c.read_message().await.map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })),
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
    public_trackers: Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    peer_tracker: Option<PeerBitfieldTracker>,
}

impl BtDownloadCommand {
    pub fn new(
        gid: GroupId,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Torrent parse failed: {}", e))))?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = meta.info.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let group = RequestGroup::new(gid, vec![format!("bt://{}", meta.info_hash.as_hex())], options.clone());

        let seed_time = options.seed_time.map(|t| if t == 0 { None } else { Some(Duration::from_secs(t)) }).flatten();
        let seed_ratio = options.seed_ratio.filter(|&r| r > 0.0);

        info!("BtDownloadCommand created: {} -> {} ({} bytes, {} pieces) seed={:?} ratio={:?}",
            meta.info.name, path.display(), meta.total_size(), meta.num_pieces(),
            seed_time, seed_ratio);

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0) > 0 || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            dht_engine: None,
            public_trackers: None,
            peer_tracker: None,
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
        ));

        let upload_limit = { self.group.read().await.options().max_upload_limit };
        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: upload_limit,
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let exit_cond = match (self.seed_time, self.seed_ratio) {
            (Some(t), Some(r)) => SeedExitCondition { seed_time: Some(t), seed_ratio: Some(r) },
            (Some(t), None) => SeedExitCondition { seed_time: Some(t), seed_ratio: None },
            (None, Some(r)) => SeedExitCondition { seed_time: None, seed_ratio: Some(r) },
            (None, None) => SeedExitCondition::infinite(),
        };

        let plain_connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = connections
            .into_iter()
            .filter_map(|c| match c {
                BtPeerConn::Plain(p) => Some(p),
                _ => None,
            })
            .collect();

        let mut manager = BtSeedManager::new(plain_connections, file_provider, config, exit_cond, self.completed_bytes);
        manager.run_seeding_loop().await?;

        self.total_uploaded = manager.total_uploaded();
        info!("Seeding complete: uploaded {} bytes in {:?}", self.total_uploaded, manager.seeding_duration());
        Ok(())
    }
}

struct FileBackedPieceProvider {
    file_path: std::path::PathBuf,
    piece_length: u32,
    num_pieces: u32,
}

impl FileBackedPieceProvider {
    pub fn new(file_path: std::path::PathBuf, piece_length: u32, num_pieces: u32) -> Self {
        Self { file_path, piece_length, num_pieces }
    }
}

impl PieceDataProvider for FileBackedPieceProvider {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        use std::io::SeekFrom;

        let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;

        let rt = tokio::runtime::Handle::try_current().ok()?;
        rt.block_on(async {
            let mut f = File::open(&self.file_path).await.ok()?;
            f.seek(SeekFrom::Start(file_pos)).await.ok()?;
            let mut buf = vec![0u8; length as usize];
            f.read_exact(&mut buf).await.ok()?;
            Some(buf)
        })
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
                std::fs::create_dir_all(parent)
                    .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e))))?;
            }
        }

        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&self.torrent_data)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("Torrent parse error: {}", e))))?;

        {
            let mut g = self.group.write().await;
            g.set_total_length(meta.total_size()).await;
        }

        let piece_length = meta.info.piece_length;
        let total_size = meta.total_size();
        let num_pieces = meta.num_pieces();

        let mut piece_manager = aria2_protocol::bittorrent::piece::manager::PieceManager::new(
            num_pieces as u32, piece_length, total_size, meta.info.pieces.clone(),
        );

        let mut piece_picker = aria2_protocol::bittorrent::piece::picker::PiecePicker::new(num_pieces as u32);
        piece_picker.set_strategy(aria2_protocol::bittorrent::piece::picker::PieceSelectionStrategy::Sequential);

        let my_peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();
        let info_hash_raw = meta.info_hash.bytes;

        let announce_url = format!("{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
            meta.announce,
            urlencode_infohash(&info_hash_raw),
            urlencode_infohash(&my_peer_id),
            total_size,
        );

        eprintln!("[BT] Announcing to tracker: {}", announce_url);
        let resp = reqwest::get(&announce_url).await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker HTTP failed: {}", e) }))?;
        eprintln!("[BT] Tracker response status: {}", resp.status());
        let body = resp.bytes().await
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker body read failed: {}", e) }))?;
        eprintln!("[BT] Tracker body: {:?}", String::from_utf8_lossy(&body));

        let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: format!("Tracker parse failed: {}", e) }))?;

        eprintln!("[BT] Tracker response: {} peers", tracker_resp.peer_count());
        for peer in &tracker_resp.peers {
            eprintln!("[BT]   Peer: {}:{}", peer.ip, peer.port);
        }

        if tracker_resp.is_failure() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: tracker_resp.failure_reason.unwrap_or_default() }));
        }

        let mut peer_addrs: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> = tracker_resp.peers.iter()
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
                ..Default::default()
            };
            match aria2_protocol::bittorrent::dht::engine::DhtEngine::start(dht_config).await {
                Ok(engine) => {
                    self.dht_engine = Some(engine);
                    eprintln!("[BT] DHT engine started");
                    self.dht_engine.as_ref().unwrap().start_maintenance_loop();
                }
                Err(e) => { warn!("[BT] DHT engine start failed: {}", e); }
            }
        }

        if let Some(ref engine) = self.dht_engine {
            let result = engine.find_peers(&info_hash_raw).await;
            if !result.peers.is_empty() {
                let before = peer_addrs.len();
                for addr in &result.peers {
                    let ip_str = addr.ip().to_string();
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip_str, addr.port());
                    if !peer_addrs.iter().any(|p| p.ip == paddr.ip && p.port == paddr.port) {
                        peer_addrs.push(paddr);
                    }
                }
                eprintln!("[BT] DHT discovered {} extra peers (total: {}, contacted {} DHT nodes)",
                         peer_addrs.len() - before, peer_addrs.len(), result.nodes_contacted);
            } else {
                debug!("[BT] DHT find_peers returned no peers");
            }
        }

        let enable_public_trackers = { self.group.read().await.options().enable_public_trackers };
        if enable_public_trackers && self.public_trackers.is_none() && peer_addrs.len() < 15 {
            let ptl = std::sync::Arc::new(aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new());
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
                match Self::announce_to_public_tracker(url, &info_hash_raw, &my_peer_id, total_size).await {
                    Ok(peers) => {
                        announced += 1;
                        extra_peers.extend(peers);
                    }
                    Err(e) => { debug!("[BT] Public tracker {} failed: {}", url, e); }
                }
            }

            if !extra_peers.is_empty() {
                let before = peer_addrs.len();
                for (ip, port) in extra_peers {
                    let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port);
                    if !peer_addrs.iter().any(|p| p.ip == paddr.ip && p.port == paddr.port) {
                        peer_addrs.push(paddr);
                    }
                }
                eprintln!("[BT] Public trackers discovered {} extra peers (announced to {} of {})",
                         peer_addrs.len() - before, announced, http_urls.len());
            } else if announced > 0 {
                debug!("[BT] Public trackers responded but no peers found");
            }
        }

        if peer_addrs.is_empty() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: "No peers from tracker or DHT".into() }));
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
                    eprintln!("[BT] Connected to peer {}:{} (encrypted={})", addr.ip, addr.port, conn.is_encrypted());
                    conn.send_unchoke().await.ok();
                    conn.send_interested().await.ok();

                    let bf_len = (num_pieces + 7) / 8;
                    let empty_bf = vec![0u8; bf_len];
                    conn.send_bitfield(empty_bf.clone()).await.ok();

                    tokio::time::sleep(Duration::from_millis(100)).await;

                    eprintln!("[BT] Waiting for unchoke from {}:{}", addr.ip, addr.port);
                    for _ in 0..50 {
                        match tokio::time::timeout(Duration::from_secs(5), conn.read_message()).await {
                            Ok(Ok(Some(msg))) => {
                                use aria2_protocol::bittorrent::message::types::BtMessage;
                                if matches!(msg, BtMessage::Unchoke) { 
                                    eprintln!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                                    break; 
                                }
                                eprintln!("[BT] Got message: {:?}", msg);
                            }
                            Ok(Ok(None)) => { eprintln!("[BT] EOF from peer"); break; }
                            Ok(Err(e)) => { eprintln!("[BT] Error reading from peer: {}", e); break; }
                            Err(_) => { eprintln!("[BT] Timeout reading from peer"); break; }
                        }
                    }

                    active_connections.push(conn);
                }
                Err(e) => { eprintln!("[BT] Failed to connect peer {}: {}", addr.ip, e); continue; }
            }
        }

        eprintln!("[BT] Active connections: {}", active_connections.len());
        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: "All peer connections failed".into() }));
        }

        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let rate_limit = { let g = self.group.read().await; g.options().max_download_limit };
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => {
                Box::new(ThrottledWriter::new(raw_writer, RateLimiter::new(&RateLimiterConfig::new(Some(rate), None))))
            }
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

        eprintln!("[BT] Piece selection strategy: RarestFirst, {} pieces total, {} peers tracked",
                 num_pieces, peer_tracker.peer_count());

        loop {
            if piece_picker.is_complete() { break; }

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
            }.map(|v| v as usize);

            let next_piece_idx = match next_piece_idx {
                Some(idx) => idx,
                None => {
                    eprintln!("[BT] No piece available, waiting...");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            eprintln!("[BT] Downloading piece {}...", next_piece_idx);
            let actual_piece_len = if next_piece_idx == num_pieces - 1 && total_size % piece_length as u64 != 0 {
                (total_size % piece_length as u64) as u32
            } else {
                piece_length
            };

            let num_blocks = (actual_piece_len + BLOCK_SIZE - 1) / BLOCK_SIZE;
            eprintln!("[BT] Piece {} has {} blocks (size: {} bytes)", next_piece_idx, num_blocks, actual_piece_len);
            let mut piece_ok = false;

            for _retry in 0..MAX_RETRIES {
                eprintln!("[BT] Retry {} for piece {}", _retry, next_piece_idx);
                let mut piece_data = Vec::with_capacity(actual_piece_len as usize);
                let mut blocks_received = 0u32;

                for block_idx in 0..num_blocks {
                    let offset = block_idx * BLOCK_SIZE;
                    let len = if offset + BLOCK_SIZE > actual_piece_len { actual_piece_len - offset } else { BLOCK_SIZE };

                    let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                        index: next_piece_idx as u32,
                        begin: offset,
                        length: len,
                    };

                    eprintln!("[BT] Requesting block {} offset={} len={}", block_idx, offset, len);
                    let mut got_block = false;
                    for conn in &mut active_connections {
                        if conn.send_request(req.clone()).await.is_err() { continue; }

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
                    eprintln!("[BT] All blocks received for piece {}, verifying...", next_piece_idx);
                    if piece_manager.verify_piece_hash(next_piece_idx as u32, &piece_data) {
                        eprintln!("[BT] Piece {} verified OK", next_piece_idx);
                        piece_manager.mark_piece_complete(next_piece_idx as u32);
                        piece_picker.mark_completed(next_piece_idx as u32);
                        writer.write(&piece_data).await.ok();

                        for conn in &mut active_connections {
                            conn.send_have(next_piece_idx as u32).await.ok();
                        }
                        piece_ok = true;
                        break;
                    } else {
                        eprintln!("[BT] SHA1 mismatch on piece {}, retrying...", next_piece_idx);
                    }
                } else {
                    eprintln!("[BT] Incomplete piece {}, received {}/{}", next_piece_idx, blocks_received, num_blocks);
                }
            }

            if !piece_ok {
                eprintln!("[BT] Piece {} failed after {} retries", next_piece_idx, MAX_RETRIES);
                return Err(Aria2Error::Fatal(FatalError::Config(format!("Piece {} download failed after {} retries", next_piece_idx, MAX_RETRIES))));
            }

            {
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;

                let elapsed = last_speed_update.elapsed();
                if elapsed.as_millis() >= 500 {
                    let delta = self.completed_bytes - last_completed;
                    let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                    g.update_speed(speed, 0).await;
                    last_speed_update = Instant::now();
                    last_completed = self.completed_bytes;
                }
            }
        }

        eprintln!("[BT] Finalizing writer...");
        writer.finalize().await.ok();
        eprintln!("[BT] Writer finalized OK");
        info!("BT download done: {} ({} bytes)", self.output_path.display(), self.completed_bytes);

        if self.seed_enabled && !active_connections.is_empty() {
            eprintln!("[BT] Starting seeding phase with {} peers...", active_connections.len());
            info!("Starting seeding phase with {} peers...", active_connections.len());
            self.run_seeding_phase(active_connections, piece_length, num_pieces as u32).await?;
        } else {
            eprintln!("[BT] Skipping seeding (enabled={}, connections={})", self.seed_enabled, active_connections.len());
            for conn in &mut active_connections {
                drop(conn);
            }
        }

        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 { (self.completed_bytes as f64 / elapsed) as u64 } else { 0 }
        };
        {
            let mut g = self.group.write().await;
            g.update_progress(self.completed_bytes).await;
            g.update_speed(final_speed, self.total_uploaded).await;
            g.complete().await?;
        }

        info!("BT command done: downloaded={} uploaded={}", self.completed_bytes, self.total_uploaded);

        if let Some(ref engine) = self.dht_engine {
            if let Err(e) = engine.announce_peer(&info_hash_raw, 0).await {
                warn!("[BT] DHT announce failed: {}", e);
            } else {
                info!("[BT] DHT announce_peer sent for {}", meta.info_hash.as_hex());
            }
            engine.shutdown();
        }

        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 { CommandStatus::Running } else { CommandStatus::Pending }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl BtDownloadCommand {
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

        let resp = client.get(&url)
            .send().await
            .map_err(|e| format!("request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let body = resp.bytes().await
            .map_err(|e| format!("read body: {}", e))?;

        let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
            .map_err(|e| format!("parse response: {}", e))?;

        if tracker_resp.is_failure() {
            return Err(tracker_resp.failure_reason.unwrap_or_else(|| "tracker failure".to_string()));
        }

        Ok(tracker_resp.peers.into_iter().map(|p| (p.ip, p.port)).collect())
    }
}

fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}
