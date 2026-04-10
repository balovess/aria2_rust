use std::sync::Arc;
use tracing::{info};

use crate::engine::bt_choke_manager::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};
use crate::engine::bt_piece_downloader::write_piece_to_multi_files;
use crate::engine::bt_tracker_comm::announce_to_public_tracker;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::command::Command;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::engine::multi_file_layout::MultiFileLayout;

pub use crate::engine::bt_message_handler::{
    BLOCK_SIZE, MAX_RETRIES, BLOCK_REQUEST_TIMEOUT_SECS, MAX_BLOCK_READ_MESSAGES,
};
pub use crate::engine::bt_peer_interaction::{
    PEER_CONNECTION_DELAY_MS, MAX_UNCHOKE_WAIT_ATTEMPTS, PEER_MESSAGE_TIMEOUT_SECS,
};
pub use crate::engine::bt_piece_selector::ENDGAME_THRESHOLD;

const COMMAND_TIMEOUT_SECS: u64 = 600;
const SPEED_UPDATE_INTERVAL_MS: u128 = 500;
pub(crate) const PUBLIC_TRACKER_PEER_THRESHOLD: usize = 15;
pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

pub struct BtDownloadCommand {
    pub(crate) group: Arc<tokio::sync::RwLock<RequestGroup>>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed_bytes: u64,
    pub(crate) torrent_data: Vec<u8>,
    pub(crate) seed_enabled: bool,
    pub(crate) seed_time: Option<std::time::Duration>,
    pub(crate) seed_ratio: Option<f64>,
    pub(crate) total_uploaded: u64,
    pub(crate) udp_client: Option<crate::engine::udp_tracker_client::SharedUdpClient>,
    pub(crate) dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    peer_tracker: Option<aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker>,
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
                    Some(std::time::Duration::from_secs(t))
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

    pub fn on_peer_choke(&mut self, peer_idx: usize) {
        on_peer_choke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_peer_unchoke(&mut self, peer_idx: usize) {
        on_peer_unchoke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        on_data_received_from_peer(&mut self.choking_algo, peer_idx, bytes);
    }

    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        check_snubbed_peers(&mut self.choking_algo)
    }

    pub fn add_peer_to_tracking(
        &mut self,
        peer_id: [u8; 8],
        addr: std::net::SocketAddr,
    ) -> usize {
        add_peer_to_tracking(&mut self.choking_algo, peer_id, addr)
    }

    pub fn select_best_peer_for_request(&self) -> Option<usize> {
        select_best_peer_for_request(&self.choking_algo)
    }

    pub async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()> {
        handle_snubbed_peer(&mut self.choking_algo, peer_idx)
            .await
            .map_err(|_| Aria2Error::Fatal(FatalError::Config(format!(
                "Failed to handle snubbed peer {}", peer_idx
            ))))
    }

    pub fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        on_piece_received(&mut self.choking_algo, peer_idx, bytes);
    }

    pub async fn announce_to_public_tracker(
        tracker_url: &str,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        total_size: u64,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        announce_to_public_tracker(tracker_url, info_hash, peer_id, total_size).await
    }

    pub async fn write_piece_to_multi_files(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &[u8],
        piece_length: u32,
    ) -> Result<()> {
        write_piece_to_multi_files(layout, piece_idx, piece_data, piece_length).await
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout.as_ref().map_or(false, |l| l.is_multi_file())
    }

    pub fn get_multi_file_layout(&self) -> Option<&MultiFileLayout> {
        self.multi_file_layout.as_ref()
    }
}
