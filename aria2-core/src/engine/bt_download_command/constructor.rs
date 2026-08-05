use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::constants;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::http_tracker_client::TrackerState;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::file_lock::DownloadPathLock;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

use super::BtDownloadCommand;

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

        // BEP 0027 (Private Torrent): capture the private flag at parse time.
        // When true, the engine must disable DHT, PEX, LPD and public tracker
        // announcement to honour the privacy contract.
        let is_private = meta.is_private();
        if is_private {
            info!(
                "[BT] Private torrent detected (BEP 0027): DHT/PEX/LPD and public trackers will be disabled"
            );
        }

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

        // Set BT metadata for session persistence (Task 3)
        group.set_bt_metadata(
            meta.num_pieces() as u32,
            meta.info.piece_length,
            meta.info_hash.as_hex(),
        );

        // Create DownloadContext from torrent metadata and set TorrentAttribute.
        // In C++ aria2, this is done by bittorrent_helper::processRootDictionary()
        // which calls ctx->setAttribute(CTX_ATTR_BT, torrent) with all torrent
        // metadata fields. We replicate this here.
        {
            use crate::download::DownloadContext;
            use crate::download::download_context::{
                BtFileMode, ContextAttributeType, TorrentAttribute,
            };

            let total_size = meta.total_size();
            let piece_length = meta.info.piece_length;
            let file_path_str = path.to_string_lossy().to_string();

            // Create DownloadContext with piece length, total size, and output path
            let mut ctx = DownloadContext::new(piece_length, total_size, file_path_str);

            // Set piece hashes from torrent info dict (sha-1 hashes in hex format)
            let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
            ctx.set_piece_hashes("sha-1".to_string(), piece_hashes_hex);

            // Build TorrentAttribute from torrent metadata (C++ TorrentAttribute fields)
            let bt_file_mode = if meta.is_single_file() {
                BtFileMode::Single
            } else {
                BtFileMode::Multi
            };
            let torrent_attr = TorrentAttribute {
                name: meta.info.name.clone(),
                mode: bt_file_mode,
                announce_list: meta.announce_list.clone(),
                nodes: Vec::new(), // DHT nodes not parsed from TorrentMeta yet
                info_hash: meta.info_hash.as_hex(),
                metadata: Vec::new(), // Regular torrent has metadata on disk, not via ut_metadata
                metadata_size: 0,
                private_torrent: meta.is_private(),
                creation_date: meta.creation_date.unwrap_or(0),
                comment: meta.comment.clone().unwrap_or_default(),
                created_by: meta.created_by.clone().unwrap_or_default(),
                url_list: meta.web_seeds.clone(),
            };

            ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(torrent_attr));

            group.set_download_context(std::sync::Arc::new(ctx));
        }

        let seed_time = options.seed_time.and_then(|t| {
            if t == 0.0 {
                None
            } else {
                Some(std::time::Duration::from_secs_f64(t))
            }
        });
        let seed_ratio = options.seed_ratio.filter(|&r| r > 0.0);

        let choking_algo = if options.bt_max_upload_slots.is_some()
            || options.bt_optimistic_unchoke_interval.is_some()
            || options.bt_snubbed_timeout.is_some()
        {
            let config = ChokingConfig {
                max_upload_slots: options
                    .bt_max_upload_slots
                    .unwrap_or(constants::BT_DEFAULT_MAX_UPLOAD_SLOTS as u32)
                    as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(constants::BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS),
                snubbed_timeout_secs: options
                    .bt_snubbed_timeout
                    .unwrap_or(constants::BT_SNUBBED_TIMEOUT_SECS),
                choke_rotation_interval_secs: constants::BT_CHOKE_ROTATION_INTERVAL_SECS,
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
                        "MultiFileLayout creation failed: {}",
                        e
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

        // Acquire download path lock (J6): prevents concurrent instances from
        // writing to the same output directory. If acquisition fails, log a
        // warning but do not fail the download -- the lock is a best-effort guard.
        // NOTE: always pass the output DIRECTORY, not the file path. For
        // single-file torrents effective_output_path is dir/filename (a file
        // path); passing it to acquire_for_download would cause create_dir_all to
        // create filename as a directory, which then makes File::create fail
        // with "Access denied" (os error 5) on Windows.
        let download_path_lock =
            match DownloadPathLock::acquire_for_download(std::path::Path::new(&dir)) {
                Ok(lock) => Some(lock),
                Err(e) => {
                    warn!(
                        "Failed to acquire download path lock: {}. Proceeding without lock.",
                        e
                    );
                    None
                }
            };

        let progress = group.progress.clone();
        Ok(Self {
            local_peer_id: aria2_protocol::bittorrent::peer::id::generate_peer_id(),
            group: Arc::new(std::sync::RwLock::new(group)),
            progress,
            output_path: effective_output_path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0.0) > 0.0
                || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            tracker_announcer: None,
            dht_engine: None,
            public_trackers: None,
            choking_algo,
            multi_file_layout,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| "none".to_string()),
            secure_falloc: options.secure_falloc,
            check_integrity: options.check_integrity,
            hash_check_only: options.hash_check_only,

            // P1/P2 integration field defaults (all None, backward compatible)
            progress_manager: None,
            progress_save_interval: Duration::from_secs(60),
            lpd_manager: None,
            hook_manager: None,

            // PEX integration fields default values
            pex_known_peers: Vec::new(),
            pex_last_send_time: None,
            pex_send_interval: Duration::from_secs(60),

            // BEP 6 Fast Extension tracking
            allowed_fast_sent_peers: HashMap::new(),
            suggest_sent_counts: HashMap::new(),

            // Endgame mode default values
            endgame_state: super::super::bt_download_execute::EndgameState::new(),

            // Tracker event state machine default
            tracker_state: TrackerState::new(),

            // Web-seed URLs (extracted from torrent url-list field)
            web_seed_urls: meta.web_seeds.clone(),
            // Web seed manager (initialized lazily when needed)
            web_seed_manager: None,

            // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
            dht_periodic_lookup: super::super::bt_download_execute::execute::DhtPeriodicLookup::new(
            ),

            // Download path lock (J6)
            download_path_lock,

            // Seeding mode
            seed_manager: None,

            // BEP 0027 (Private Torrent) enforcement flag
            is_private,

            // BtRegistry integration (set via set_bt_registry after construction)
            bt_registry: None,

            // Process-wide rate limiter (set via set_global_limiter after construction)
            global_limiter: None,

            peer_rejection: crate::engine::bt_peer_storage::PeerRejectionState::shared(),
        })
    }
}
