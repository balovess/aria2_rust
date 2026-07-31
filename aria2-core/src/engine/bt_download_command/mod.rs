mod choke_api;
mod constructor;
mod integration_api;
mod peer_ban_api;
mod seed_api;
mod web_seed_api;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::http_tracker_client::TrackerState;
use crate::engine::lpd_manager::LpdManager;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::request::request_group::{AtomicProgress, RequestGroup};

pub use crate::engine::bt_message_handler::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, MAX_BLOCK_READ_MESSAGES, MAX_RETRIES,
};
pub use crate::engine::bt_peer_interaction::{
    MAX_UNCHOKE_WAIT_ATTEMPTS, PEER_CONNECTION_DELAY_MS, PEER_MESSAGE_TIMEOUT_SECS,
};
pub use crate::engine::bt_piece_selector::ENDGAME_THRESHOLD;

// Re-export sub-module public items
pub use seed_api::SeedStats;

pub(crate) const PUBLIC_TRACKER_PEER_THRESHOLD: usize = 15;
pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

pub struct BtDownloadCommand {
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed_bytes: u64,
    pub(crate) torrent_data: Vec<u8>,
    pub(crate) seed_enabled: bool,
    pub(crate) seed_time: Option<std::time::Duration>,
    pub(crate) seed_ratio: Option<f64>,
    pub(crate) total_uploaded: u64,
    pub(crate) udp_client: Option<crate::engine::udp_tracker_client::SharedUdpClient>,
    /// Unified tracker announcer (HTTP + UDP) using BtAnnounce state machine.
    /// Created during execute() from the torrent announce list.
    pub(crate) tracker_announcer: Option<crate::engine::bt_tracker_comm::TrackerAnnouncer>,
    pub(crate) dht_engine:
        Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    pub(crate) choking_algo: Option<ChokingAlgorithm>,
    pub(crate) multi_file_layout: Option<MultiFileLayout>,

    /// File allocation strategy from options
    /// ("none" / "prealloc" / "falloc" / "trunc" / "mmap"). Mirrors C++
    /// `FileAllocationEntry` choosing an iterator from `PREF_FILE_ALLOCATION`.
    pub(crate) file_allocation: String,
    /// Zero-fill after fallocate on platforms that don't zero-fill.
    pub(crate) secure_falloc: bool,
    /// `--check-integrity`: verify existing data against piece hashes before
    /// downloading (C++ `CheckIntegrityMan`).
    pub(crate) check_integrity: bool,

    // P1/P2 integration fields (all use Option for backward compatibility)
    /// BT progress persistence manager
    pub(crate) progress_manager: Option<crate::engine::bt_progress_info_file::BtProgressManager>,
    /// Progress save interval (default 60 seconds)
    pub(crate) progress_save_interval: Duration,
    /// LPD LAN peer discovery manager
    pub(crate) lpd_manager: Option<Arc<LpdManager>>,
    /// Post-download handler manager
    pub(crate) hook_manager: Option<Arc<crate::engine::hook_manager::HookManager>>,

    // PEX (Peer Exchange, BEP 11) integration fields
    /// Track known peers for PEX exchange
    pub(crate) pex_known_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Timestamp of last PEX message sent (for rate limiting)
    pub(crate) pex_last_send_time: Option<Instant>,
    /// Interval between PEX messages (default 60 seconds)
    pub(crate) pex_send_interval: Duration,

    // Endgame mode (Phase 14 - B1/B2): duplicate request tracking for final pieces
    /// Tracks duplicate block requests during endgame mode
    pub(crate) endgame_state: super::bt_download_execute::EndgameState,

    // BEP 6 (Fast Extension): track AllowedFast messages sent to peers
    /// Track which AllowedFast pieces have been sent to each peer
    /// Key: peer identifier (using connection index for now)
    #[allow(dead_code)]
    pub(crate) allowed_fast_sent_peers: HashMap<usize, HashSet<u32>>,

    /// Track suggest counts per peer to avoid spamming
    pub(crate) suggest_sent_counts: HashMap<usize, usize>,

    // Tracker event state machine (Phase 15 - H5): manages Started/Completed/Stopped events
    /// State machine for tracker announce events
    #[allow(dead_code)]
    pub(crate) tracker_state: TrackerState,

    // Web-seed (BEP 19 / HTTP fallback) integration
    /// URLs extracted from torrent url-list field for HTTP piece download fallback
    pub(crate) web_seed_urls: Vec<String>,
    /// Web seed manager for HTTP piece downloads (initialized on first use)
    pub(crate) web_seed_manager: Option<crate::engine::bt_web_seed::WebSeedManager>,

    // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
    /// Tracks timing and retry state for periodic DHT get_peers lookups.
    /// C++: DHTGetPeersCommand runs as a per-torrent command that
    /// triggers DHT lookups at adaptive intervals (15min normal,
    /// 5min low peers, 1min zero peers, 5s retry).
    /// TODO: Wire into BT download loop for periodic DHT peer discovery.
    #[allow(dead_code)]
    pub(crate) dht_periodic_lookup: super::bt_download_execute::execute::DhtPeriodicLookup,

    // File lock (J6): prevents concurrent aria2 instances from writing to same output dir
    /// Download path lock held for the lifetime of this command.
    /// Prevents other aria2 instances from writing to the same output directory.
    pub download_path_lock: Option<crate::filesystem::file_lock::DownloadPathLock>,

    // Seeding mode (Phase 16 - Complete BitTorrent seeding)
    /// Seed manager for uploading after download completes
    pub(crate) seed_manager: Option<super::bt_seed_manager::BtSeedManager>,

    // BEP 0027 (Private Torrent): when true, DHT/PEX/LPD and public tracker
    // announcement are disabled to enforce the privacy guarantees of the
    // torrent private flag.
    pub(crate) is_private: bool,

    // BtRegistry integration: the command registers itself into the engine BtRegistry
    // during execute() so that info-hash reverse lookup, peer
    // blocklist, and cross-download coordination work end-to-end.
    // Set via set_bt_registry() after construction by the engine or caller.
    pub(crate) bt_registry: Option<Arc<std::sync::RwLock<super::bt_registry::BtRegistry>>>,
}

impl BtDownloadCommand {
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        use crate::util::rwlock_ext::RwLockRecover;
        self.group.recover()
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout
            .as_ref()
            .is_some_and(|l| l.is_multi_file())
    }

    pub fn get_multi_file_layout(&self) -> Option<&MultiFileLayout> {
        self.multi_file_layout.as_ref()
    }

    /// Wrapper around crate::engine::bt_piece_downloader::write_piece_to_multi_files.
    pub async fn write_piece_to_multi_files(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &[u8],
        piece_length: u32,
    ) -> crate::error::Result<()> {
        crate::engine::bt_piece_downloader::write_piece_to_multi_files(
            layout,
            piece_idx,
            piece_data,
            piece_length,
        )
        .await
    }

    /// Wrapper around crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced.
    ///
    /// Prefer this over write_piece_to_multi_files for production use -- it
    /// merges adjacent writes within a 4 KiB gap, reducing syscall count.
    pub async fn write_piece_to_multi_files_coalesced(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &bytes::Bytes,
        piece_length: u32,
    ) -> crate::error::Result<()> {
        crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced(
            layout,
            piece_idx,
            piece_data,
            piece_length,
        )
        .await
    }

    pub async fn announce_to_public_tracker(
        tracker_url: &str,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        total_size: u64,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        crate::engine::bt_tracker_comm::announce_to_public_tracker(
            tracker_url,
            info_hash,
            peer_id,
            total_size,
        )
        .await
    }
}
