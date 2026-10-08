use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::constants;
use crate::engine::bittorrent::peer::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::bittorrent::torrent::file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::file_lock::DownloadPathLock;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::{BtFileMapping, DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::BtDownloadCommand;
use super::torrent_context::{
    apply_file_mappings, apply_index_out_paths, apply_select_file_filter,
    build_download_context_from_meta,
};

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

    /// Construct a command with the engine's outbound source policy already
    /// installed, so the shared uTP socket is bound correctly at creation.
    pub(crate) fn new_with_group_and_mappings_with_policy(
        group: std::sync::Arc<std::sync::RwLock<RequestGroup>>,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
        file_mappings: &[BtFileMapping],
        policy: &crate::network::OutboundNetworkPolicy,
    ) -> Result<Self> {
        Self::new_with_group_and_mappings_inner(
            group,
            torrent_bytes,
            options,
            output_dir,
            file_mappings,
            policy,
        )
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
        Self::new_with_group_and_mappings_inner(
            group,
            torrent_bytes,
            options,
            output_dir,
            file_mappings,
            &crate::network::OutboundNetworkPolicy::direct(),
        )
    }

    fn new_with_group_and_mappings_inner(
        group: std::sync::Arc<std::sync::RwLock<RequestGroup>>,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
        file_mappings: &[BtFileMapping],
        policy: &crate::network::OutboundNetworkPolicy,
    ) -> Result<Self> {
        let gid = group.recover().gid();
        let live_max_peers = group.recover().bt_max_peers_limit();
        let mut command = Self::new_with_policy(gid, torrent_bytes, options, output_dir, policy)?;
        let current_info_hash = command.group.recover().get_bt_info_hash_hex();
        let existing_context = group.recover().get_download_context();
        let parsed_context = if existing_context.as_ref().is_some_and(|context| {
            context.get_bt_info_hash_hex().is_some_and(|info_hash| {
                current_info_hash
                    .as_deref()
                    .is_some_and(|current| info_hash.eq_ignore_ascii_case(current))
            })
        }) {
            // A dependency may have already installed a context carrying
            // Metalink-selected paths and mirrors. Reuse it only when it
            // belongs to this exact torrent; a stale session context must not
            // leak piece hashes or output mappings into a new torrent.
            existing_context
        } else if file_mappings.is_empty() {
            command.group.recover().get_download_context()
        } else {
            let meta =
                aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
                    .map_err(|error| {
                        Aria2Error::BittorrentParse(format!("Torrent parse failed: {error}"))
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
            let mut context = build_download_context_from_meta(&meta, context_path, &[])?;
            apply_index_out_paths(&mut context, options.index_out.as_deref(), &dir)?;
            apply_file_mappings(&mut context, file_mappings)?;
            apply_select_file_filter(&mut context, options.select_file.as_deref())?;
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
            external.set_rate_limiter(command.torrent_upload_limiter.clone());
        }
        command.group = group;
        command.bt_runtime = std::sync::Arc::new(super::BtRuntimeState::new(live_max_peers));
        command.progress = command.group.recover().progress.clone();
        command.total_uploaded = command.group.recover().get_uploaded_length();
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
        Self::new_with_policy(
            gid,
            torrent_bytes,
            options,
            output_dir,
            &crate::network::OutboundNetworkPolicy::direct(),
        )
    }

    pub(crate) fn new_with_policy(
        gid: GroupId,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
        policy: &crate::network::OutboundNetworkPolicy,
    ) -> Result<Self> {
        let (meta, local_metadata) =
            aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse_with_info_bytes(
                torrent_bytes,
            )
            .map_err(|error| {
                Aria2Error::BittorrentParse(format!("Torrent parse failed: {error}"))
            })?;
        let local_metadata: Arc<[u8]> = local_metadata.into();

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
        let torrent_upload_limiter =
            RateLimiter::new(&RateLimiterConfig::new(None, options.max_upload_limit));
        group.set_rate_limiter(torrent_upload_limiter.clone());

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
        let mut ctx =
            build_download_context_from_meta(&meta, path.to_string_lossy().to_string(), &[])?;
        apply_index_out_paths(&mut ctx, options.index_out.as_deref(), &dir)?;
        apply_select_file_filter(&mut ctx, options.select_file.as_deref())?;
        group.set_download_context(std::sync::Arc::new(ctx));

        let seed_time = options.seed_time.and_then(|t| {
            if t <= 0.0 || !t.is_finite() {
                None
            } else {
                // aria2_original treats seed-time as fractional minutes and
                // truncates the converted value to whole seconds.
                Some(std::time::Duration::from_secs((t * 60.0).floor() as u64))
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

        let multi_file_root = std::path::PathBuf::from(&dir).join(&filename);
        let multi_file_layout = if !meta.is_single_file() {
            let layout_base_dir = multi_file_root.clone();
            match MultiFileLayout::from_info_dict(&meta.info, &layout_base_dir) {
                Ok(layout) => Some(layout),
                Err(e) => {
                    return Err(Aria2Error::BittorrentParse(format!(
                        "MultiFileLayout creation failed: {}",
                        e
                    )));
                }
            }
        } else {
            None
        };

        let effective_output_path = if multi_file_layout.is_some() {
            multi_file_root
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
        let peer_storage = {
            let mut storage = crate::engine::bittorrent::peer::storage::DefaultPeerStorage::new();
            if let Some(path) = options.bt_peer_blocklist.as_deref() {
                let mut blocklist =
                    crate::engine::bittorrent::peer::blocklist::BtPeerBlocklist::new();
                blocklist
                    .load_from_file(std::path::Path::new(path))
                    .map_err(|error| Aria2Error::Fatal(FatalError::Config(error)))?;
                storage.set_peer_blocklist(Arc::new(blocklist));
            }
            Arc::new(std::sync::Mutex::new(storage))
        };
        let bt_runtime =
            std::sync::Arc::new(super::BtRuntimeState::new(group.bt_max_peers_limit()));
        let mut command = Self {
            local_peer_id: aria2_protocol::bittorrent::peer::id::generate_peer_id_with_prefix(
                &options.peer_id_prefix,
            ),
            group: Arc::new(std::sync::RwLock::new(group)),
            progress,
            output_path: effective_output_path,
            started: false,
            started_at: None,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            local_metadata,
            // An explicit seed-time=0 is the original way to disable
            // seeding, even though seed-ratio has a positive default.
            seed_enabled: options.seed_time != Some(0.0)
                && (options.seed_time.unwrap_or(0.0) > 0.0
                    || options.seed_ratio.unwrap_or(0.0) > 0.0),
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            tracker_actor: None,
            listen_port: 0,
            bt_runtime,
            peer_coordinator: crate::engine::bittorrent::peer::coordinator::BtPeerCoordinator::new(
                options.bt_max_peers,
                10,
            ),
            initial_peer_swarm: None,
            dht_engines: crate::engine::bittorrent::dht::engine_set::DhtEngineSet::default(),
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
            bt_enable_hook_after_hash_check: options.bt_enable_hook_after_hash_check,
            bt_hash_check_seed: options.bt_hash_check_seed,
            bt_seed_unverified: options.bt_seed_unverified,
            hash_check_completed: false,
            bt_complete_event_emitted: false,

            // P1/P2 integration field defaults (all None, backward compatible)
            progress_manager: None,
            progress_save_interval: Duration::from_secs(60),
            // LPD is process-wide. The engine/task spawner injects its one
            // shared manager after construction.
            lpd_manager: None,
            lpd_registered_info_hash: None,
            hook_manager: None,

            // PEX integration fields default values
            pex_known_peers: Vec::new(),
            pex_last_send_time: None,
            pex_send_interval: Duration::from_secs(60),

            // Endgame mode default values

            // Web seed manager (initialized lazily when needed)

            // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
            dht_periodic_lookup: super::super::execute::DhtPeriodicLookup::new(),

            // Download path lock (J6)
            download_path_lock,
            output_path_reservation: None,

            // Seeding mode

            // BEP 0027 (Private Torrent) enforcement flag
            is_private,

            // BtRegistry integration (set via set_bt_registry after construction)
            bt_registry: None,
            tracker_runtime: None,

            // Process-wide rate limiter (set via set_global_limiter after construction)
            global_limiter: None,
            torrent_upload_limiter,
            outbound_network_policy: Arc::new(policy.clone()),

            peer_rejection: crate::engine::bittorrent::peer::storage::PeerRejectionState::shared(),
            peer_storage,
            incoming_peers: None,
            utp_transport: None,
            // Direct command users do not pass through DownloadEngine's
            // dependency injector. Give that public construction path a
            // listener manager; the engine replaces it with its shared
            // process-level manager before execution.
            bt_listener: Some(std::sync::Arc::new(
                crate::engine::bittorrent::peer::listener::BtPeerListenerManager::new(),
            )),
            bt_peer_route: None,
            checkpoint: None,
            checkpoint_bytes_since_save: 0,
            checkpoint_last_save: Instant::now(),
            dirty_multi_file_indices: std::collections::HashSet::new(),
        };
        command.apply_context_paths()?;
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::{BtDownloadCommand, DownloadOptions, GroupId};

    #[test]
    fn with_policy_constructor_installs_the_policy_for_peer_connections() {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let source = "127.0.0.1"
            .parse()
            .expect("test source address should parse");
        let policy = crate::network::OutboundNetworkPolicy::single(source);
        let command = BtDownloadCommand::new_with_policy(
            GroupId::new(902),
            &torrent,
            &DownloadOptions::default(),
            None,
            &policy,
        )
        .expect("BT command should construct with an outbound policy");

        assert_eq!(command.outbound_network_policy.addresses(), vec![source]);
    }
}
