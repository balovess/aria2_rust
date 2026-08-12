use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::config::parse_index_out;
use crate::constants;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::http_tracker_client::TrackerState;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::file_lock::DownloadPathLock;
use crate::request::request_group::{BtFileMapping, DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::BtDownloadCommand;

/// Build the protocol-specific context that aria2 installs after torrent
/// metadata has been resolved.
///
/// Keeping this separate from command construction lets a dependency resolve
/// torrent metadata into an existing payload RequestGroup.
pub(crate) fn build_download_context_from_meta(
    meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    path: String,
) -> crate::error::Result<crate::download::DownloadContext> {
    use crate::download::DownloadContext;
    use crate::download::download_context::{BtFileMode, ContextAttributeType, TorrentAttribute};
    use crate::download::file_entry::FileEntry;

    let mut ctx = if meta.is_single_file() {
        DownloadContext::new(meta.info.piece_length, meta.total_size(), path)
    } else {
        let base_dir = std::path::Path::new(&path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut entries = Vec::with_capacity(meta.info.files.as_ref().map_or(0, Vec::len));
        let mut offset = 0u64;
        for torrent_file in meta.info.files.as_deref().unwrap_or_default() {
            let original_name = torrent_file.path.join("/");
            let file_path = base_dir.join(std::path::Path::new(&original_name));
            let mut entry = FileEntry::new(
                file_path.to_string_lossy().into_owned(),
                torrent_file.length,
                offset,
                Vec::new(),
            );
            entry.set_original_name(original_name.clone());
            entry.set_suffix_path(original_name);
            entries.push(entry);
            offset = offset.saturating_add(torrent_file.length);
        }
        let mut context = DownloadContext::new_default();
        context.set_piece_length(meta.info.piece_length);
        context.set_file_entries(entries);
        context
    };
    if meta.is_single_file()
        && let Some(entry) = ctx.get_file_entries_mut().first_mut()
    {
        entry.set_original_name(meta.info.name.clone());
        entry.set_suffix_path(meta.info.name.clone());
    }
    let piece_hashes_hex: Vec<String> = meta.info.pieces.iter().map(hex::encode).collect();
    ctx.set_piece_hashes("sha-1".to_string(), piece_hashes_hex);

    let torrent_attr = TorrentAttribute {
        name: meta.info.name.clone(),
        mode: if meta.is_single_file() {
            BtFileMode::Single
        } else {
            BtFileMode::Multi
        },
        announce_list: meta.announce_list.clone(),
        nodes: Vec::new(),
        info_hash: meta.info_hash.as_hex(),
        metadata: Vec::new(),
        metadata_size: 0,
        private_torrent: meta.is_private(),
        creation_date: meta.creation_date.unwrap_or(0),
        comment: meta.comment.clone().unwrap_or_default(),
        created_by: meta.created_by.clone().unwrap_or_default(),
        url_list: meta.web_seeds.clone(),
    };
    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(torrent_attr));
    Ok(ctx)
}

/// Apply Metalink-selected paths and mirrors to a parsed torrent context.
///
/// The torrent parser owns the canonical file order and byte offsets. This
/// helper only changes the selected entries' destination and URI metadata, so
/// both dependency resolution and command fallback use the same mapping rule.
pub(crate) fn apply_file_mappings(
    context: &mut crate::download::DownloadContext,
    mappings: &[BtFileMapping],
) -> Result<()> {
    if mappings.is_empty() {
        return Ok(());
    }

    let entries = context.get_file_entries_mut();
    if entries.len() == 1 && mappings.len() == 1 && mappings[0].original_name.is_empty() {
        apply_file_mapping(&mut entries[0], &mappings[0]);
        return Ok(());
    }

    for entry in entries.iter_mut() {
        entry.set_requested(false);
    }

    for mapping in mappings {
        let entry = entries
            .iter_mut()
            .find(|entry| entry.original_name() == mapping.original_name)
            .ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "No entry '{}' in torrent metadata",
                    mapping.original_name
                )))
            })?;
        apply_file_mapping(entry, mapping);
    }
    Ok(())
}

fn apply_file_mapping(entry: &mut crate::download::file_entry::FileEntry, mapping: &BtFileMapping) {
    entry.set_requested(true);
    entry.set_path(mapping.path.clone());
    entry.set_uris(&mapping.uris);
    entry.set_max_connection_per_server(mapping.max_connection_per_server);
    entry.set_unique_protocol(mapping.unique_protocol);
}

fn apply_index_out_paths(
    context: &mut crate::download::DownloadContext,
    index_out: Option<&str>,
    dir: &str,
) -> Result<()> {
    let Some(index_out) = index_out else {
        return Ok(());
    };

    for (index, suffix_path) in
        parse_index_out(index_out).map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?
    {
        let path = std::path::Path::new(dir).join(suffix_path);
        context
            .set_file_path_with_index(index, path.to_string_lossy().into_owned())
            .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?;
    }
    Ok(())
}

impl BtDownloadCommand {
    /// Construct a BitTorrent command while retaining an externally managed
    /// RequestGroup owned by RequestGroupMan.
    pub fn new_with_group(
        group: std::sync::Arc<std::sync::RwLock<RequestGroup>>,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_group_and_mappings(group, torrent_bytes, options, output_dir, &[])
    }

    /// Construct a command for an externally owned group and remap selected
    /// torrent entries to Metalink output paths and mirrors.
    pub(crate) fn new_with_group_and_mappings(
        group: std::sync::Arc<std::sync::RwLock<RequestGroup>>,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
        file_mappings: &[BtFileMapping],
    ) -> Result<Self> {
        let gid = group.recover().gid();
        let mut command = Self::new(gid, torrent_bytes, options, output_dir)?;
        let parsed_context = if file_mappings.is_empty() {
            command.group.recover().get_download_context()
        } else {
            let meta =
                aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
                    .map_err(|error| {
                        Aria2Error::Fatal(FatalError::Config(format!(
                            "Torrent parse failed: {error}"
                        )))
                    })?;
            let dir = output_dir
                .map(str::to_owned)
                .or_else(|| options.dir.clone())
                .unwrap_or_else(|| ".".to_string());
            let context_path = if meta.is_single_file() {
                command.output_path.to_string_lossy().into_owned()
            } else {
                std::path::Path::new(&dir)
                    .join(&meta.info.name)
                    .to_string_lossy()
                    .into_owned()
            };
            let mut context = build_download_context_from_meta(&meta, context_path)?;
            apply_index_out_paths(&mut context, options.index_out.as_deref(), &dir)?;
            apply_file_mappings(&mut context, file_mappings)?;
            Some(std::sync::Arc::new(context))
        };
        let (piece_count, piece_length, info_hash) = {
            let temporary = command.group.recover();
            (
                temporary.get_bt_num_pieces(),
                temporary.get_bt_piece_length(),
                temporary.get_bt_info_hash_hex(),
            )
        };
        {
            let external = group.recover();
            if let Some(context) = parsed_context {
                external.set_download_context(context);
            }
            if let Some(info_hash) = info_hash {
                external.set_bt_metadata(piece_count, piece_length, info_hash);
            }
        }
        command.group = group;
        command.progress = command.group.recover().progress.clone();
        command.apply_context_paths()?;
        Ok(command)
    }

    /// Apply paths from an externally prepared context, such as a Metalink
    /// torrent dependency. Torrent piece offsets stay unchanged while the
    /// destination files follow the Metalink mapping.
    fn apply_context_paths(&mut self) -> Result<()> {
        let paths = self
            .group
            .recover()
            .get_download_context()
            .map(|context| {
                context
                    .get_file_entries()
                    .iter()
                    .map(|entry| std::path::PathBuf::from(entry.path()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if let Some(layout) = self.multi_file_layout.as_mut() {
            if paths.len() == layout.num_files() {
                for (index, path) in paths.into_iter().enumerate() {
                    layout
                        .set_file_absolute_path(index, path)
                        .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?;
                }
            }
        } else if let Some(path) = paths.into_iter().next()
            && !path.as_os_str().is_empty()
        {
            self.output_path = path;
        }
        Ok(())
    }

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
        let mut ctx = build_download_context_from_meta(&meta, path.to_string_lossy().to_string())?;
        apply_index_out_paths(&mut ctx, options.index_out.as_deref(), &dir)?;
        group.set_download_context(std::sync::Arc::new(ctx));

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
        let mut command = Self {
            local_peer_id: aria2_protocol::bittorrent::peer::id::generate_peer_id(),
            group: Arc::new(std::sync::RwLock::new(group)),
            progress,
            output_path: effective_output_path,
            started: false,
            started_at: None,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0.0) > 0.0
                || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            tracker_announcer: None,
            listen_port: 0,
            bt_runtime: std::sync::Arc::new(super::BtRuntimeState::new(options.bt_max_peers)),
            peer_coordinator: crate::engine::bt_peer_coordinator::BtPeerCoordinator::new(
                options.bt_max_peers,
                10,
            ),
            dht_engine: None,
            public_trackers: None,
            choking_algo,
            multi_file_layout,
            file_allocation: options
                .file_allocation
                .clone()
                .unwrap_or_else(|| crate::constants::DEFAULT_FILE_ALLOCATION.to_string()),
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
            peer_storage: std::sync::Arc::new(std::sync::Mutex::new(
                crate::engine::bt_peer_storage::DefaultPeerStorage::new(),
            )),
            incoming_peers: None,
            incoming_peer_listener_task: None,
        };
        command.apply_context_paths()?;
        Ok(command)
    }
}
