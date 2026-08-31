//! Core-owned implementation of the protocol-independent RPC backend.
//!
//! `aria2-rpc` owns the wire protocol and knows only [`RpcBackend`].  This
//! adapter is the single place where RPC operations are translated into
//! `aria2-core` state changes, queries, and engine commands.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aria2_core::checksum::checksum::Checksum;
use aria2_core::config::{
    ConfigManager, OptionRegistry, is_global_option_changeable, project_initial_options,
};
use aria2_core::engine::command::Command;
use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::{ChangePositionMode, RequestGroupMan};
use aria2_core::session::save_session_command::SaveSessionCommand;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{
    BackendError, BackendEvent, BackendMetadata, BackendReadSnapshot, BackendRequest,
    BackendResponse, BackendResult, FileInfo, GlobalStat, PeerInfo, PositionMode, RpcBackend,
    ServerInfo, ServerInfoIndex, StatusInfo, UriEntry, UriStatus,
};
use async_trait::async_trait;
use tokio::sync::RwLock;

const RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn rpc_peer_port(addr: SocketAddr, is_incoming: bool) -> u16 {
    if is_incoming { 0 } else { addr.port() }
}

/// The application adapter behind the RPC wire layer.
pub struct CoreRpcBackend {
    group_man: Arc<RequestGroupMan>,
    engine_cmd_tx: EngineCommandSender,
    config: Arc<RwLock<ConfigManager>>,
    save_session_path: Option<PathBuf>,
    metadata: BackendMetadata,
}

impl CoreRpcBackend {
    pub fn new(
        group_man: Arc<RequestGroupMan>,
        engine_cmd_tx: EngineCommandSender,
        config: Arc<RwLock<ConfigManager>>,
        save_session_path: Option<PathBuf>,
        product_version: impl Into<String>,
    ) -> Self {
        let mut metadata = BackendMetadata::base(product_version);
        #[cfg(feature = "bittorrent")]
        {
            metadata = metadata.with_bittorrent();
        }
        #[cfg(feature = "metalink")]
        {
            metadata = metadata.with_metalink();
        }
        #[cfg(feature = "sftp")]
        {
            metadata = metadata.with_sftp();
        }

        Self {
            group_man,
            engine_cmd_tx,
            config,
            save_session_path,
            metadata,
        }
    }

    fn invalid(message: impl Into<String>) -> BackendError {
        BackendError::InvalidParams(message.into())
    }

    fn execution(message: impl Into<String>) -> BackendError {
        BackendError::Execution(message.into())
    }

    fn parse_gid(gid: &str) -> Result<GroupId, BackendError> {
        GroupId::from_hex_string(gid).ok_or_else(|| Self::invalid("Invalid GID"))
    }

    fn send(&self, command: EngineCommand) -> Result<(), BackendError> {
        self.engine_cmd_tx.send(command).map_err(|error| {
            BackendError::Internal(format!("Failed to send engine command: {error}"))
        })
    }

    fn group(&self, gid: &str) -> Result<Arc<std::sync::RwLock<RequestGroup>>, BackendError> {
        self.group_man
            .group_by_hex(gid)
            .ok_or_else(|| Self::execution(format!("GID {gid} not found")))
    }

    async fn global_options(&self) -> HashMap<String, serde_json::Value> {
        self.config
            .read()
            .await
            .get_all_global_options()
            .await
            .into_iter()
            .map(|(key, value)| (key, (&value).into()))
            .collect()
    }

    async fn merged_task_options(
        &self,
        request_options: HashMap<String, serde_json::Value>,
    ) -> Result<(DownloadOptions, HashMap<String, serde_json::Value>), BackendError> {
        let mut options = self.global_options().await;
        options.extend(request_options);
        let download_options =
            DownloadOptions::try_from_rpc_options(&options).map_err(Self::invalid)?;
        let snapshot = project_initial_options(options);
        if let Some((algorithm, value)) = &download_options.checksum {
            Checksum::from_type_and_value(algorithm, value)
                .map_err(|error| Self::invalid(format!("Invalid checksum: {error}")))?;
        }
        Ok((download_options, snapshot))
    }

    fn add_group(
        &self,
        gid: GroupId,
        uris: Vec<String>,
        options: DownloadOptions,
        option_snapshot: HashMap<String, serde_json::Value>,
        torrent_data: Option<Vec<u8>>,
    ) -> Result<String, BackendError> {
        self.group_man
            .add_group_with_gid(gid, uris, options)
            .map_err(|error| Self::execution(format!("Failed to add group: {error}")))?;
        let group = self
            .group_man
            .group_by_id(gid)
            .ok_or_else(|| BackendError::Internal("Group not found after insert".into()))?;
        {
            let mut group = group
                .write()
                .map_err(|_| BackendError::Internal("Failed to lock request group".into()))?;
            group.set_option_snapshot(option_snapshot);
            #[cfg(feature = "bittorrent")]
            if let Some(data) = torrent_data {
                group.set_bt_metadata_data(data);
            }
        }
        self.send(EngineCommand::AddDownload { group })?;
        Ok(gid.to_hex_string())
    }

    async fn add_uri(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        let (download_options, snapshot) = self.merged_task_options(options).await?;
        let gid = self.group_man.next_available_gid();
        let gid_hex = self.add_group(gid, uris, download_options, snapshot, None)?;
        if let Some(position) = position {
            self.change_position(&gid_hex, position as i32, PositionMode::SetFromStart)?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid_hex.clone()),
            vec![BackendEvent::DownloadStart(gid_hex)],
        ))
    }

    async fn add_torrent(
        &self,
        data: Vec<u8>,
        additional_uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = (data, additional_uris, options, position);
            return Err(BackendError::Unsupported(
                "BitTorrent is not enabled".into(),
            ));
        }

        #[cfg(feature = "bittorrent")]
        {
            let (download_options, snapshot) = self.merged_task_options(options).await?;
            let gid = self.group_man.next_available_gid();
            let mut uris = Vec::with_capacity(1 + additional_uris.len());
            uris.push(format!("bt://{}", gid.to_hex_string()));
            uris.extend(additional_uris);
            let gid_hex = self.add_group(gid, uris, download_options, snapshot, Some(data))?;
            if let Some(position) = position {
                self.change_position(&gid_hex, position as i32, PositionMode::SetFromStart)?;
            }
            Ok(BackendResult::with_events(
                BackendResponse::Gid(gid_hex.clone()),
                vec![BackendEvent::DownloadStart(gid_hex)],
            ))
        }
    }

    async fn add_metalink(
        &self,
        data: Vec<u8>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        #[cfg(not(feature = "metalink"))]
        {
            let _ = (data, options, position);
            Err(BackendError::Unsupported("Metalink is not enabled".into()))
        }

        #[cfg(feature = "metalink")]
        {
            let (download_options, snapshot) = self.merged_task_options(options).await?;
            let converter =
                aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup::new();
            let mut gids = std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
            let resource_groups = converter
                .create_resource_groups_from_bytes(&data, &download_options, &mut gids)
                .map_err(|error| Self::invalid(error.to_string()))?;
            let mut response_gids = Vec::new();
            let mut start_gids = Vec::new();
            for group in resource_groups {
                let gid = group.recover().gid();
                group.recover_mut().set_option_snapshot(snapshot.clone());
                let wake_group = Arc::clone(&group);
                self.group_man.add_group_arc(group);
                self.send(EngineCommand::AddDownload { group: wake_group })?;
                let gid = gid.to_hex_string();
                response_gids.push(gid.clone());
                start_gids.push(gid);
            }

            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            {
                let mut graph_gids =
                    std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
                let graphs = converter
                    .create_torrent_graphs_from_bytes(&data, &download_options, &mut graph_gids)
                    .map_err(|error| Self::invalid(error.to_string()))?;
                for graph in graphs {
                    let metadata_gid = graph.metadata.recover().gid();
                    let payload_gid = graph.payload.recover().gid();
                    graph
                        .metadata
                        .recover_mut()
                        .set_option_snapshot(snapshot.clone());
                    graph
                        .payload
                        .recover_mut()
                        .set_option_snapshot(snapshot.clone());
                    let metadata_group = Arc::clone(&graph.metadata);
                    let payload_group = Arc::clone(&graph.payload);
                    self.group_man
                        .add_metalink_graph(graph)
                        .map_err(|error| Self::execution(error.to_string()))?;
                    // The manager insertion above makes the groups visible to
                    // RPC reads immediately. These idempotent commands wake a
                    // running engine so it promotes the newly inserted queue.
                    self.send(EngineCommand::AddDownload {
                        group: metadata_group,
                    })?;
                    self.send(EngineCommand::AddDownload {
                        group: payload_group,
                    })?;
                    let metadata_gid = metadata_gid.to_hex_string();
                    let payload_gid = payload_gid.to_hex_string();
                    response_gids.extend([metadata_gid.clone(), payload_gid.clone()]);
                    start_gids.extend([metadata_gid, payload_gid]);
                }
            }

            if let Some(position) = position
                && let Some(gid) = response_gids.first()
            {
                self.change_position(gid, position as i32, PositionMode::SetFromStart)?;
            }
            Ok(BackendResult::with_events(
                BackendResponse::Gids(response_gids),
                start_gids
                    .into_iter()
                    .map(BackendEvent::DownloadStart)
                    .collect(),
            ))
        }
    }

    fn change_position(
        &self,
        gid: &str,
        position: i32,
        mode: PositionMode,
    ) -> Result<BackendResult, BackendError> {
        let gid = Self::parse_gid(gid)?;
        let mode = match mode {
            PositionMode::SetFromStart => ChangePositionMode::SetFromStart,
            PositionMode::MoveFromStart => ChangePositionMode::MoveFromStart,
            PositionMode::SetFromEnd => ChangePositionMode::SetFromEnd,
        };
        let position = self
            .group_man
            .change_position(gid, position, mode)
            .map_err(|error| Self::execution(error.to_string()))?;
        Ok(BackendResult::response(BackendResponse::Position(position)))
    }

    fn lifecycle_gids(&self) -> Vec<String> {
        self.group_man
            .all_groups()
            .into_iter()
            .map(|(_, group)| group.recover().gid().to_hex_string())
            .collect()
    }

    fn pause(&self, gid: String, force: bool) -> Result<BackendResult, BackendError> {
        let parsed = Self::parse_gid(&gid)?;
        if force {
            self.group_man
                .force_pause_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.send(EngineCommand::ForcePause { gid: parsed })?;
        } else {
            self.group_man
                .pause_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.send(EngineCommand::Pause { gid: parsed })?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadPause(gid)],
        ))
    }

    fn unpause(&self, gid: String) -> Result<BackendResult, BackendError> {
        let parsed = Self::parse_gid(&gid)?;
        self.group_man
            .unpause_group(parsed)
            .map_err(|error| Self::execution(error.to_string()))?;
        self.send(EngineCommand::Unpause { gid: parsed })?;
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadStart(gid)],
        ))
    }

    fn remove(&self, gid: String, force: bool) -> Result<BackendResult, BackendError> {
        let parsed = Self::parse_gid(&gid)?;
        let enqueue = if force {
            self.group_man
                .force_remove_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.group_man.find_group(parsed).is_some()
        } else {
            self.group_man
                .remove_group(parsed)
                .map_err(|error| Self::execution(error.to_string()))?;
            self.group_man.find_group(parsed).is_some()
        };
        if enqueue {
            self.send(if force {
                EngineCommand::ForceRemoveDownload { gid: parsed }
            } else {
                EngineCommand::RemoveDownload { gid: parsed }
            })?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadStop(gid)],
        ))
    }

    fn status_from_group(group: &RequestGroup, gid: &str) -> StatusInfo {
        let snapshot = group.status_snapshot();
        let status = map_status(snapshot.status.clone());
        let bt = snapshot.bt.as_ref();
        let mut info = StatusInfo::new(gid)
            .with_status(status.clone())
            .with_total_length(snapshot.total_length)
            .with_completed_length(snapshot.completed_length)
            .with_upload_length(snapshot.upload_length)
            .with_download_speed(snapshot.download_speed)
            .with_upload_speed(snapshot.upload_speed)
            .with_connections(u16::try_from(snapshot.connections).unwrap_or(u16::MAX))
            .with_dir(group.options().dir.clone().unwrap_or_default())
            .with_files(build_file_infos(group, snapshot.completed_length));

        if let Some(bt) = bt {
            info = info
                .with_info_hash(bt.info_hash.clone())
                .with_num_seeders(bt.seeder_count() as u32)
                .with_num_pieces(bt.num_pieces)
                .with_piece_length(bt.piece_length as u64)
                .with_completed_pieces(bt.completed_pieces)
                .with_missing_pieces(bt.missing_pieces);
            if let Some(bitfield) = &bt.bitfield {
                info = info.with_bitfield(
                    bitfield
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                );
            }
        }
        if info.piece_length.is_none() && snapshot.total_length > 0 {
            info = info.with_piece_length(1_048_576);
        }
        if info.num_pieces.is_none() && snapshot.total_length > 0 {
            let piece_length = info.piece_length.unwrap_or(1_048_576);
            if piece_length > 0 {
                info = info.with_num_pieces(snapshot.total_length.div_ceil(piece_length) as u32);
            }
        }
        match status {
            aria2_rpc::DownloadStatus::Error(message) => {
                info.with_error_code(1).with_error_message(message)
            }
            aria2_rpc::DownloadStatus::Complete => info.with_error_code(0),
            aria2_rpc::DownloadStatus::Removed => info.with_error_code(31),
            _ => info,
        }
    }

    fn status_from_result(
        result: &aria2_core::request::request_group::DownloadResult,
    ) -> StatusInfo {
        let mut info = StatusInfo::new(result.gid_hex())
            .with_status(map_status(result.status.clone()))
            .with_total_length(result.total_length)
            .with_completed_length(result.completed_length)
            .with_upload_length(result.upload_length)
            .with_download_speed(result.download_speed)
            .with_upload_speed(result.upload_speed)
            .with_error_code(result.code.as_code() as i32)
            .with_error_message(result.message.clone())
            .with_dir(result.dir.clone());
        if !result.files.is_empty() {
            info = info.with_files(build_file_infos_from_result(result));
        }
        info
    }

    fn capture_snapshot(&self) -> BackendReadSnapshot {
        let active = self
            .group_man
            .get_active_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let waiting = self
            .group_man
            .get_waiting_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let stopped = self
            .group_man
            .get_stopped_results(0, usize::MAX)
            .iter()
            .map(Self::status_from_result)
            .collect::<Vec<_>>();
        let global_stat = global_stat(&active, &waiting, stopped.len());
        BackendReadSnapshot {
            active,
            waiting,
            stopped,
            global_stat,
        }
    }

    fn tell_status(&self, gid: String) -> Result<BackendResult, BackendError> {
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let group = group.recover();
            return Ok(BackendResult::response(BackendResponse::Status(
                Self::status_from_group(&group, &gid),
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Status(
                Self::status_from_result(&result),
            )));
        }
        Err(Self::execution(format!("GID {gid} not found")))
    }

    fn tell_active(&self, keys: Vec<String>) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_active_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(statuses)))
    }

    fn tell_waiting(
        &self,
        offset: i64,
        num: usize,
        keys: Vec<String>,
    ) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_waiting_groups()
            .into_iter()
            .map(|group| {
                let group = group.recover();
                Self::status_from_group(&group, &group.gid().to_hex_string())
            })
            .collect::<Vec<_>>();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(
            paginate(statuses, offset, num),
        )))
    }

    fn tell_stopped(
        &self,
        offset: i64,
        num: usize,
        keys: Vec<String>,
    ) -> Result<BackendResult, BackendError> {
        let statuses = self
            .group_man
            .get_stopped_results(0, usize::MAX)
            .iter()
            .map(Self::status_from_result)
            .collect::<Vec<_>>();
        let _ = keys;
        Ok(BackendResult::response(BackendResponse::Statuses(
            paginate(statuses, offset, num),
        )))
    }

    async fn change_global_option(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<BackendResult, BackendError> {
        let changes: HashMap<_, _> = changes
            .into_iter()
            .filter(|(key, _)| is_global_option_changeable(key))
            .collect();
        let registry = OptionRegistry::new();
        let mut parsed = Vec::with_capacity(changes.len());
        for (key, value) in &changes {
            let value = registry
                .parse_rpc_value(key, value)
                .map_err(|error| Self::execution(format!("Option '{key}': {error}")))?;
            parsed.push((key.clone(), value));
        }

        // Complete all adapter-specific validation before touching the shared
        // configuration. This keeps a rejected request from partially
        // applying an unrelated option in the same RPC batch.
        let max_concurrent = parsed
            .iter()
            .find(|(key, _)| key == "max-concurrent-downloads")
            .map(|(_, value)| {
                let value = value.as_i64().ok_or_else(|| {
                    Self::execution("Option 'max-concurrent-downloads' must be an integer")
                })?;
                u32::try_from(value)
                    .map_err(|_| Self::execution("Option 'max-concurrent-downloads' is too large"))
            })
            .transpose()?;

        let runtime_rate_limits = if changes.contains_key("max-overall-download-limit")
            || changes.contains_key("max-overall-upload-limit")
        {
            let options = self.global_options().await;
            Some((
                parse_rate_limit(
                    changes
                        .get("max-overall-download-limit")
                        .or_else(|| options.get("max-overall-download-limit")),
                    "max-overall-download-limit",
                )?,
                parse_rate_limit(
                    changes
                        .get("max-overall-upload-limit")
                        .or_else(|| options.get("max-overall-upload-limit")),
                    "max-overall-upload-limit",
                )?,
            ))
        } else {
            None
        };

        #[cfg(feature = "bittorrent")]
        let tracker_sources = changes
            .get("bt-tracker-source")
            .map(|value| {
                let sources = rpc_value_to_string(value).ok_or_else(|| {
                    Self::execution("Option 'bt-tracker-source' must be a string or array")
                })?;
                if sources
                    .split([',', '\n'])
                    .map(str::trim)
                    .all(|source| source.is_empty())
                {
                    return Err(Self::execution(
                        "Option 'bt-tracker-source' must contain at least one source",
                    ));
                }
                Ok(sources)
            })
            .transpose()?;

        #[cfg(feature = "bittorrent")]
        let tracker_update_interval = changes
            .get("bt-tracker-update-interval")
            .map(|value| {
                let seconds = parse_u64(value, "bt-tracker-update-interval")?;
                if seconds == 0 {
                    return Err(Self::execution(
                        "Option 'bt-tracker-update-interval' must be greater than zero",
                    ));
                }
                Ok(seconds)
            })
            .transpose()?;

        #[cfg(feature = "bittorrent")]
        let public_trackers_enabled = parsed
            .iter()
            .find(|(key, _)| key == "enable-public-trackers")
            .map(|(_, value)| {
                value
                    .as_bool()
                    .ok_or_else(|| Self::execution("enable-public-trackers must be boolean"))
            })
            .transpose()?;

        {
            let mut config = self.config.write().await;
            for (key, value) in &parsed {
                config
                    .set_global_option(key, value.clone())
                    .await
                    .map_err(Self::execution)?;
            }
        }

        if let Some(max) = max_concurrent {
            self.send(EngineCommand::SetMaxConcurrent { max })?;
        }
        if let Some((download_limit, upload_limit)) = runtime_rate_limits {
            self.send(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            })?;
        }

        #[cfg(feature = "bittorrent")]
        {
            if let Some(sources) = tracker_sources {
                self.send(EngineCommand::SetPublicTrackerSources { sources })?;
            }
            if let Some(seconds) = tracker_update_interval {
                self.send(EngineCommand::SetPublicTrackerUpdateInterval { seconds })?;
            }
            if let Some(enabled) = public_trackers_enabled {
                self.send(EngineCommand::SetPublicTrackersEnabled { enabled })?;
            }
        }
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }

    async fn get_option(&self, gid: String) -> Result<BackendResult, BackendError> {
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let (snapshot, runtime) = {
                let group = group.recover();
                (group.effective_option_snapshot(), group.runtime_options())
            };
            if let Some(options) = snapshot {
                return Ok(BackendResult::response(BackendResponse::Options(options)));
            }
            if !runtime.is_empty() {
                return Ok(BackendResult::response(BackendResponse::Options(runtime)));
            }
            return Ok(BackendResult::response(BackendResponse::Options(
                self.global_options().await,
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Options(
                result.option_snapshot().cloned().unwrap_or_default(),
            )));
        }
        Err(Self::execution(format!("GID {gid} not found")))
    }

    fn get_peers(&self, gid: String) -> Result<BackendResult, BackendError> {
        let group = self.group(&gid)?;
        let peers = group
            .recover()
            .status_snapshot()
            .bt
            .map(|bt| bt.peers)
            .unwrap_or_default()
            .into_iter()
            .map(|peer| PeerInfo {
                peer_id: peer
                    .peer_id
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                ip: peer.addr.ip().to_string(),
                port: rpc_peer_port(peer.addr, peer.is_incoming),
                bitfield: None,
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                download_speed: peer.download_speed.max(0.0) as u64,
                upload_speed: peer.upload_speed.max(0.0) as u64,
                seeder: peer.seeder.map(|value| value.to_string()),
            })
            .collect();
        Ok(BackendResult::response(BackendResponse::Peers(peers)))
    }

    fn get_uris(&self, gid: String) -> Result<BackendResult, BackendError> {
        let group = self.group(&gid)?;
        let entries = group
            .recover()
            .uri_entries()
            .into_iter()
            .map(|entry| UriEntry {
                uri: entry.uri,
                status: match entry.status.as_str() {
                    "used" | "spent" => UriStatus::Used,
                    _ => UriStatus::Waiting,
                },
            })
            .collect();
        Ok(BackendResult::response(BackendResponse::Uris(entries)))
    }

    fn get_files(&self, gid: String) -> Result<BackendResult, BackendError> {
        if let Some(group) = self.group_man.group_by_hex(&gid) {
            let group = group.recover();
            return Ok(BackendResult::response(BackendResponse::Files(
                build_file_infos(&group, group.get_completed_length()),
            )));
        }
        if let Some(result) = self.group_man.find_stopped_result(&gid) {
            return Ok(BackendResult::response(BackendResponse::Files(
                build_file_infos_from_result(&result),
            )));
        }
        Err(Self::execution(format!(
            "No file data is available for GID#{gid}"
        )))
    }

    fn get_servers(&self, gid: String) -> Result<BackendResult, BackendError> {
        let group = self.group(&gid)?;
        let group = group.recover();
        if !matches!(group.status(), DownloadStatus::Active) {
            return Err(Self::execution(format!("No active download for GID#{gid}")));
        }
        let servers = group
            .get_download_context()
            .map(|context| {
                context
                    .get_file_entries()
                    .iter()
                    .enumerate()
                    .map(|(index, file)| ServerInfoIndex {
                        index: index + 1,
                        servers: file
                            .in_flight_requests()
                            .iter()
                            .filter_map(|request| {
                                let stats = request.peer_stat()?;
                                Some(
                                    ServerInfo::new(request.uri())
                                        .with_current_uri(request.current_uri())
                                        .with_download_speed(stats.download_speed),
                                )
                            })
                            .collect(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(BackendResult::response(BackendResponse::Servers(servers)))
    }

    fn change_option(
        &self,
        gid: String,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<BackendResult, BackendError> {
        self.group_man
            .change_group_options(&gid, normalize_options(&options))
            .map_err(Self::execution)?;
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }
}

#[async_trait]
impl RpcBackend for CoreRpcBackend {
    fn metadata(&self) -> BackendMetadata {
        self.metadata.clone()
    }

    async fn task_count(&self) -> usize {
        self.group_man.count()
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        match request {
            BackendRequest::AddUri {
                uris,
                options,
                position,
            } => self.add_uri(uris, options, position).await,
            BackendRequest::AddTorrent {
                data,
                additional_uris,
                options,
                position,
            } => {
                self.add_torrent(data, additional_uris, options, position)
                    .await
            }
            BackendRequest::AddMetalink {
                data,
                options,
                position,
            } => self.add_metalink(data, options, position).await,
            BackendRequest::Remove { gid } => self.remove(gid, false),
            BackendRequest::ForceRemove { gids } => {
                let mut last = String::new();
                let mut events = Vec::with_capacity(gids.len());
                for gid in gids {
                    last = gid.clone();
                    let result = self.remove(gid, true)?;
                    events.extend(result.events);
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(last),
                    events,
                ))
            }
            BackendRequest::Pause { gid } => self.pause(gid, false),
            BackendRequest::ForcePause { gid } => self.pause(gid, true),
            BackendRequest::Unpause { gid } => self.unpause(gid),
            BackendRequest::TellStatus { gid, .. } => self.tell_status(gid),
            BackendRequest::TellActive { .. } => self.tell_active(Vec::new()),
            BackendRequest::TellWaiting { offset, num, .. } => {
                self.tell_waiting(offset, num, Vec::new())
            }
            BackendRequest::TellStopped { offset, num, .. } => {
                self.tell_stopped(offset, num, Vec::new())
            }
            BackendRequest::GetGlobalStat => {
                let snapshot = self.capture_snapshot();
                Ok(BackendResult::response(BackendResponse::GlobalStat(
                    snapshot.global_stat,
                )))
            }
            BackendRequest::GetUris { gid } => self.get_uris(gid),
            BackendRequest::GetFiles { gid } => self.get_files(gid),
            BackendRequest::GetServers { gid } => self.get_servers(gid),
            BackendRequest::PurgeDownloadResult => {
                self.group_man.purge_stopped_results();
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::RemoveDownloadResult { gid } => {
                if self.group_man.remove_stopped_result(&gid).is_none() {
                    return Err(Self::execution(format!(
                        "GID {gid} not found in download results"
                    )));
                }
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::GetGlobalOption => {
                let options = self.global_options().await;
                let options =
                    OptionRegistry::new().project_defined_global_options_for_rpc(&options);
                Ok(BackendResult::response(BackendResponse::Options(options)))
            }
            BackendRequest::ChangeGlobalOption { options } => {
                self.change_global_option(options).await
            }
            BackendRequest::GetOption { gid } => self.get_option(gid).await,
            BackendRequest::ChangeOption { gid, options } => self.change_option(gid, options),
            BackendRequest::GetPeers { gid } => self.get_peers(gid),
            BackendRequest::PauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.pause_all();
                self.send(EngineCommand::PauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadPause).collect(),
                ))
            }
            BackendRequest::ForcePauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.force_pause_all();
                self.send(EngineCommand::ForcePauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadPause).collect(),
                ))
            }
            BackendRequest::UnpauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.unpause_all();
                self.send(EngineCommand::UnpauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadStart).collect(),
                ))
            }
            BackendRequest::ChangeUri {
                gid,
                file_index,
                delete_uris,
                add_uris,
                position,
            } => {
                let group = self.group(&gid)?;
                let result = group
                    .write()
                    .map_err(|_| BackendError::Internal("Failed to lock request group".into()))?
                    .change_uris(file_index, &delete_uris, &add_uris, position)
                    .map_err(|error| Self::execution(error.to_string()))?;
                Ok(BackendResult::response(BackendResponse::Counts([
                    result.0, result.1,
                ])))
            }
            BackendRequest::SaveSession => {
                let path = self
                    .save_session_path
                    .clone()
                    .ok_or_else(|| Self::execution("Filename is not given. Set --save-session."))?;
                let mut command = SaveSessionCommand::new(path, Arc::clone(&self.group_man));
                command.execute().await.map_err(|error| {
                    BackendError::Internal(format!("Failed to save session: {error}"))
                })?;
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::ChangePosition {
                gid,
                position,
                mode,
            } => self.change_position(&gid, position, mode),
            BackendRequest::Shutdown { force } => {
                let count = self.group_man.count();
                if force {
                    self.group_man.force_remove_reserved();
                }
                aria2_core::engine::halt_watchers::spawn_timed_halt(
                    self.engine_cmd_tx.clone(),
                    if force {
                        std::time::Duration::ZERO
                    } else {
                        RPC_SHUTDOWN_GRACE
                    },
                    force,
                );
                let text = if force {
                    format!("OK. {count} downloads forcibly terminated.")
                } else {
                    format!("OK. {count} active downloads paused.")
                };
                Ok(BackendResult::response(BackendResponse::Text(text)))
            }
        }
    }

    async fn capture_read_snapshot(
        &self,
    ) -> Result<Option<Arc<BackendReadSnapshot>>, BackendError> {
        Ok(Some(Arc::new(self.capture_snapshot())))
    }

    async fn execute_with_snapshot(
        &self,
        request: BackendRequest,
        snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> Result<BackendResult, BackendError> {
        match (&request, snapshot) {
            (BackendRequest::TellActive { .. }, Some(snapshot)) => Ok(BackendResult::response(
                BackendResponse::Statuses(snapshot.active.clone()),
            )),
            (BackendRequest::TellWaiting { offset, num, .. }, Some(snapshot)) => {
                Ok(BackendResult::response(BackendResponse::Statuses(
                    paginate(snapshot.waiting.clone(), *offset, *num),
                )))
            }
            (BackendRequest::TellStopped { offset, num, .. }, Some(snapshot)) => {
                Ok(BackendResult::response(BackendResponse::Statuses(
                    paginate(snapshot.stopped.clone(), *offset, *num),
                )))
            }
            (BackendRequest::GetGlobalStat, Some(snapshot)) => Ok(BackendResult::response(
                BackendResponse::GlobalStat(snapshot.global_stat.clone()),
            )),
            _ => self.execute(request).await,
        }
    }
}

fn map_status(status: DownloadStatus) -> aria2_rpc::DownloadStatus {
    match status {
        DownloadStatus::Waiting => aria2_rpc::DownloadStatus::Waiting,
        DownloadStatus::Active => aria2_rpc::DownloadStatus::Active,
        DownloadStatus::Paused => aria2_rpc::DownloadStatus::Paused,
        DownloadStatus::Error(message) => aria2_rpc::DownloadStatus::Error(message),
        DownloadStatus::Complete => aria2_rpc::DownloadStatus::Complete,
        DownloadStatus::Removed => aria2_rpc::DownloadStatus::Removed,
    }
}

fn global_stat(active: &[StatusInfo], waiting: &[StatusInfo], stopped: usize) -> GlobalStat {
    GlobalStat {
        download_speed: active
            .iter()
            .chain(waiting)
            .filter_map(|status| status.download_speed)
            .fold(0, u64::saturating_add),
        upload_speed: active
            .iter()
            .chain(waiting)
            .filter_map(|status| status.upload_speed)
            .fold(0, u64::saturating_add),
        num_active: active.len(),
        num_waiting: waiting.len(),
        num_stopped: stopped,
        num_stopped_total: stopped,
    }
}

fn paginate<T>(items: Vec<T>, offset: i64, num: usize) -> Vec<T> {
    if num == 0 {
        return Vec::new();
    }
    let size = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let originally_negative = offset < 0;
    let (start, count) = if originally_negative {
        let end = offset.saturating_add(size);
        if end < 0 {
            return Vec::new();
        }
        let count = i64::try_from(num).unwrap_or(i64::MAX);
        let mut start = end.saturating_sub(count.saturating_sub(1));
        let count = if start < 0 {
            start = 0;
            end.saturating_add(1)
        } else {
            count
        };
        (start, count)
    } else {
        if offset >= size {
            return Vec::new();
        }
        (offset, i64::try_from(num).unwrap_or(i64::MAX))
    };
    if start < 0 || start >= size {
        return Vec::new();
    }
    let end = start.saturating_add(count).min(size).max(start);
    let mut selected = items
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect::<Vec<_>>();
    if originally_negative {
        selected.reverse();
    }
    selected
}

fn build_file_infos(group: &RequestGroup, completed: u64) -> Vec<FileInfo> {
    let fallback_path = || {
        let name = group
            .options()
            .out
            .clone()
            .or_else(|| {
                group
                    .uris()
                    .first()
                    .and_then(|uri| uri.rsplit('/').next().map(str::to_owned))
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_default();
        match group.options().dir.as_deref().filter(|dir| !dir.is_empty()) {
            Some(dir) if !name.is_empty() => PathBuf::from(dir).join(name).to_string_lossy().into(),
            _ => name,
        }
    };

    if let Some(context) = group.get_download_context() {
        return context
            .get_file_entries()
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let mut info = FileInfo::new(
                    if file.path().is_empty() {
                        fallback_path()
                    } else {
                        file.path().to_owned()
                    },
                    file.length(),
                )
                .with_index(index + 1)
                .with_completed(completed.saturating_sub(file.offset()).min(file.length()))
                .with_uris(build_uri_entries(file));
                info.selected = file.is_requested();
                info
            })
            .collect();
    }

    let mut info = FileInfo::new(fallback_path(), group.get_total_length_atomic())
        .with_index(1)
        .with_completed(completed)
        .with_uris(group.uris().iter().cloned().map(UriEntry::new).collect());
    info.selected = true;
    vec![info]
}

fn build_uri_entries(file: &aria2_core::download::file_entry::FileEntry) -> Vec<UriEntry> {
    let remaining = file.remaining_uris();
    file.uris()
        .into_iter()
        .map(|uri| UriEntry {
            status: if remaining.iter().any(|candidate| candidate == &uri) {
                UriStatus::Waiting
            } else {
                UriStatus::Used
            },
            uri,
        })
        .collect()
}

fn build_file_infos_from_result(
    result: &aria2_core::request::request_group::DownloadResult,
) -> Vec<FileInfo> {
    result
        .files
        .iter()
        .map(|file| {
            let mut info = FileInfo::new(file.path.clone(), file.length)
                .with_index(file.index)
                .with_completed(file.completed_length)
                .with_uris(
                    file.uris
                        .iter()
                        .map(|uri| UriEntry {
                            uri: uri.uri.clone(),
                            status: match uri.status.as_str() {
                                "used" | "spent" => UriStatus::Used,
                                _ => UriStatus::Waiting,
                            },
                        })
                        .collect(),
                );
            info.selected = file.selected;
            info
        })
        .collect()
}

fn normalize_options(
    options: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    options
        .iter()
        .filter_map(|(key, value)| {
            rpc_value_to_string(value).map(|value| (key.clone(), serde_json::Value::String(value)))
        })
        .collect()
}

fn rpc_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(rpc_value_to_string)
            .collect::<Option<Vec<_>>>()
            .map(|values| values.join("\n")),
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

fn parse_u64(value: &serde_json::Value, option: &str) -> Result<u64, BackendError> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .ok_or_else(|| SelfError::execution(format!("Option '{option}' must be an integer")))
}

fn parse_rate_limit(
    value: Option<&serde_json::Value>,
    option: &str,
) -> Result<Option<u64>, BackendError> {
    let value =
        value.ok_or_else(|| SelfError::execution(format!("Option '{option}' is missing")))?;
    let raw = rpc_value_to_string(value)
        .ok_or_else(|| SelfError::execution(format!("Option '{option}' must be a byte rate")))?;
    let (number, multiplier) = match raw.chars().last() {
        Some('k' | 'K') => (&raw[..raw.len() - 1], 1024u64),
        Some('m' | 'M') => (&raw[..raw.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        Some('t' | 'T') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024 * 1024),
        _ => (raw.as_str(), 1),
    };
    let number = number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| SelfError::execution(format!("Option '{option}' must be a byte rate")))?;
    let bytes = number * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return Err(SelfError::execution(format!(
            "Option '{option}' is too large"
        )));
    }
    Ok((bytes as u64 > 0).then_some(bytes as u64))
}

struct SelfError;

impl SelfError {
    fn execution(message: impl Into<String>) -> BackendError {
        BackendError::Execution(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_uses_real_non_bt_connection_count_instead_of_split() {
        let options = DownloadOptions {
            split: Some(16),
            ..DownloadOptions::default()
        };
        let group = RequestGroup::new(GroupId::new(0x101), Vec::new(), options);
        group.set_stream_connection_count(1);

        let status = CoreRpcBackend::status_from_group(&group, "0000000000000101");

        assert_eq!(status.connections, Some(1));
    }

    #[test]
    fn incoming_bt_peer_reports_aria2_compatible_zero_port() {
        let incoming_addr = "127.0.0.1:7673".parse().expect("valid socket address");
        assert_eq!(rpc_peer_port(incoming_addr, true), 0);
        assert_eq!(rpc_peer_port(incoming_addr, false), 7673);
    }
}
