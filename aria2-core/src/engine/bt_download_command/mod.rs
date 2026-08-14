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
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};

pub use crate::engine::bt_message_handler::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, MAX_BLOCK_READ_MESSAGES, MAX_RETRIES,
};
pub use crate::engine::bt_peer_interaction::{
    MAX_UNCHOKE_WAIT_ATTEMPTS, PEER_CONNECTION_DELAY_MS, PEER_MESSAGE_TIMEOUT_SECS,
};
pub use crate::engine::bt_piece_selector::ENDGAME_THRESHOLD;

// Re-export sub-module public items
pub(crate) use constructor::{apply_file_mappings, build_download_context_from_meta};
pub use seed_api::SeedStats;

pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

#[derive(Debug)]
pub(crate) struct BtRuntimeState {
    connections: std::sync::atomic::AtomicUsize,
    max_peers: std::sync::atomic::AtomicUsize,
}

impl BtRuntimeState {
    pub(crate) fn new(max_peers: usize) -> Self {
        Self {
            connections: std::sync::atomic::AtomicUsize::new(0),
            max_peers: std::sync::atomic::AtomicUsize::new(max_peers),
        }
    }

    pub(crate) fn set_connections(&self, connections: usize) {
        self.connections
            .store(connections, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn set_max_peers(&self, max_peers: usize) {
        self.max_peers
            .store(max_peers, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn max_peers(&self) -> usize {
        self.max_peers.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn min_peers(&self) -> usize {
        let max_peers = self.max_peers.load(std::sync::atomic::Ordering::Acquire);
        if max_peers == 0 {
            0
        } else {
            (max_peers * 4 / 5).max(1)
        }
    }

    pub(crate) fn less_than_min_peers(&self) -> bool {
        self.connections() < self.min_peers()
    }

    pub(crate) fn less_than_max_peers(&self) -> bool {
        self.max_peers() == 0 || self.connections() < self.max_peers()
    }
}

impl Drop for BtDownloadCommand {
    fn drop(&mut self) {
        self.bt_peer_route.take();

        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let peers: Vec<_> = storage.used_peers().iter().cloned().collect();
        for peer in peers {
            storage.return_peer(&peer);
        }
    }
}

pub struct BtDownloadCommand {
    /// Stable BitTorrent peer ID for this download session.
    pub(crate) local_peer_id: [u8; 20],
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    /// Monotonic timestamp captured when execution begins.
    pub(crate) started_at: Option<Instant>,
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
    /// Actual TCP listener port advertised to trackers for this command.
    pub(crate) listen_port: u16,
    pub(crate) bt_runtime: std::sync::Arc<BtRuntimeState>,
    pub(crate) peer_coordinator: crate::engine::bt_peer_coordinator::BtPeerCoordinator,
    pub(crate) dht_engine:
        Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    /// Public catalog entries actually appended to this command's announce list.
    pub(crate) public_tracker_urls: HashSet<String>,
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
    /// Only perform the piece hash check and terminate without peer discovery.
    pub(crate) hash_check_only: bool,
    /// Allow the BitTorrent completion hook/notification when an existing
    /// payload passes `check-integrity`.
    pub(crate) bt_enable_hook_after_hash_check: bool,
    /// Continue into the BitTorrent peer/seed lifecycle after a complete
    /// payload passes `check-integrity`.
    pub(crate) bt_hash_check_seed: bool,
    /// Whether the current command completed from an integrity check rather
    /// than by downloading missing pieces.
    pub(crate) hash_check_completed: bool,
    /// Whether the BT completion event was already emitted at the integrity
    /// check seam.
    pub(crate) bt_complete_event_emitted: bool,

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
    /// Key: stable peer identity.
    #[allow(dead_code)]
    pub(crate) allowed_fast_sent_peers:
        HashMap<super::bt_download_execute::types::PeerKey, HashSet<u32>>,

    /// Track suggest counts per peer to avoid spamming.
    pub(crate) suggest_sent_counts: HashMap<super::bt_download_execute::types::PeerKey, usize>,

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
    /// Periodic lookup state is polled from the BT piece loop.
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

    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` so that this torrent's
    /// piece writes share a single bandwidth ceiling with all concurrent
    /// downloads.
    pub(crate) global_limiter: Option<RateLimiter>,

    /// Shared rejection state for verified bad piece sources.
    pub(crate) peer_rejection: crate::engine::bt_peer_storage::SharedPeerRejection,

    /// Session-scoped peer identity pool shared by discovery and connection
    /// scheduling. Socket ownership remains in the download loop until the
    /// lifecycle adapter is wired in.
    pub(crate) peer_storage:
        std::sync::Arc<std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>>,

    /// Receiver for incoming peers routed by the engine-owned listener.
    pub(crate) incoming_peers:
        Option<tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>>,
    /// Process-level listener shared by all BitTorrent downloads.
    pub(crate) bt_listener: Option<Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>>,
    /// RAII registration for this torrent's info-hash route.
    pub(crate) bt_peer_route: Option<crate::engine::bt_peer_listener::BtPeerRouteHandle>,
    /// Rust-owned A2CF checkpoint for verified torrent pieces.
    pub(crate) checkpoint: Option<crate::engine::bt_checkpoint::BtCheckpoint>,
}

impl BtDownloadCommand {
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        use crate::util::rwlock_ext::RwLockRecover;
        self.group.recover()
    }

    pub fn group_handle(&self) -> Arc<std::sync::RwLock<RequestGroup>> {
        Arc::clone(&self.group)
    }

    /// Complete the asynchronous shutdown phase before the command is dropped.
    ///
    /// `Drop` can only reclaim synchronous resources. Callers that own the
    /// command lifecycle should await this method before aborting or dropping
    /// the task so tracker stopped announcements are not lost.
    pub async fn shutdown(&mut self) {
        if let Ok(meta) =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&self.torrent_data)
            && let Some(ref mut announcer) = self.tracker_announcer
        {
            let total_size = meta.total_size();
            announcer
                .announce_stopped(
                    &meta.info_hash.bytes,
                    &self.local_peer_id,
                    self.completed_bytes,
                    total_size.saturating_sub(self.completed_bytes),
                    self.total_uploaded,
                )
                .await;
        }
        self.bt_peer_route.take();
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, piece writes performed by this command acquire tokens from
    /// this limiter (in addition to any per-download limiter) so that all
    /// concurrent downloads share a global bandwidth ceiling.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
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

#[cfg(test)]
mod tests {
    use super::{BtDownloadCommand, BtRuntimeState};

    #[test]
    fn runtime_state_uses_the_same_min_peer_boundary_as_tracker_demand() {
        let runtime = BtRuntimeState::new(55);
        assert_eq!(runtime.min_peers(), 44);
        assert!(runtime.less_than_min_peers());

        runtime.set_connections(44);
        assert!(!runtime.less_than_min_peers());

        runtime.set_max_peers(0);
        assert_eq!(runtime.min_peers(), 0);
        assert!(!runtime.less_than_min_peers());
        assert!(runtime.less_than_max_peers());
    }

    #[test]
    fn runtime_state_accepts_runtime_max_peer_changes() {
        let runtime = BtRuntimeState::new(10);
        runtime.set_connections(7);
        assert!(runtime.less_than_min_peers());

        runtime.set_max_peers(8);
        assert!(!runtime.less_than_min_peers());
        assert_eq!(runtime.max_peers(), 8);
    }

    #[test]
    fn explicit_zero_seed_time_overrides_the_default_seed_ratio() {
        let torrent = crate::engine::bt_download_command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions {
            seed_time: Some(0.0),
            ..Default::default()
        };
        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(10),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        assert!(!command.seed_enabled);
    }

    #[test]
    fn positive_seed_options_enable_seeding() {
        let torrent = crate::engine::bt_download_command_tests::build_test_torrent();
        let options = crate::request::request_group::DownloadOptions {
            seed_time: Some(1.0),
            ..Default::default()
        };
        let command = BtDownloadCommand::new(
            crate::request::request_group::GroupId::new(11),
            &torrent,
            &options,
            None,
        )
        .expect("test torrent should construct");

        assert!(command.seed_enabled);
    }
}
