use std::path::PathBuf;

#[cfg(feature = "bittorrent")]
use aria2_core::engine::bt_tracker_comm::TrackerRuntimeSnapshot;
use aria2_core::request::request_group::{DownloadStatus, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{
    BackendError, BackendReadSnapshot, BackendResponse, BackendResult, FileInfo, GlobalStat,
    PeerInfo, ServerInfo, ServerInfoIndex, StatusInfo, UriEntry, UriStatus,
};

use super::{CoreRpcBackend, rpc_peer_port};

impl CoreRpcBackend {
    pub(super) fn status_from_group(group: &RequestGroup, gid: &str) -> StatusInfo {
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

    pub(super) fn status_from_result(
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

    pub(super) fn capture_snapshot(&self) -> BackendReadSnapshot {
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

    pub(super) fn tell_status(&self, gid: String) -> Result<BackendResult, BackendError> {
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

    pub(super) fn tell_active(&self, keys: Vec<String>) -> Result<BackendResult, BackendError> {
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

    pub(super) fn tell_waiting(
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

    pub(super) fn tell_stopped(
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

    pub(super) async fn get_option(&self, gid: String) -> Result<BackendResult, BackendError> {
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

    pub(super) fn get_peers(&self, gid: String) -> Result<BackendResult, BackendError> {
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
                source: peer.source.as_str().to_string(),
                port: rpc_peer_port(peer.addr, peer.is_incoming),
                bitfield: peer
                    .bitfield
                    .map(|bitfield| bitfield.iter().map(|byte| format!("{byte:02x}")).collect()),
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                download_speed: peer.download_speed.max(0.0) as u64,
                upload_speed: peer.upload_speed.max(0.0) as u64,
                seeder: peer.seeder.map(|value| value.to_string()),
            })
            .collect();
        Ok(BackendResult::response(BackendResponse::Peers(peers)))
    }

    #[cfg(feature = "bittorrent")]
    pub(super) fn get_trackers(&self, gid: String) -> Result<BackendResult, BackendError> {
        let registry = self
            .bt_registry
            .as_ref()
            .ok_or_else(|| Self::execution("BitTorrent registry is unavailable"))?;
        let gid = u64::from_str_radix(&gid, 16)
            .map_err(|_| Self::execution(format!("Invalid GID {gid}")))?;
        let guard = registry
            .read()
            .map_err(|_| BackendError::Internal("Failed to lock BitTorrent registry".into()))?;
        let object = guard
            .get(gid)
            .ok_or_else(|| Self::execution(format!("GID {gid:x} is not a BitTorrent task")))?;
        if let Some(runtime) = object.tracker_runtime.as_ref() {
            let snapshot = runtime.read().map_err(|_| {
                BackendError::Internal("Failed to lock tracker runtime snapshot".into())
            })?;
            return Ok(BackendResult::response(BackendResponse::Trackers(
                tracker_infos_from_runtime(&snapshot),
            )));
        }
        let announce = object
            .bt_announce
            .as_ref()
            .ok_or_else(|| Self::execution("Tracker announcer is unavailable"))?;
        let current_announce = announce.as_ref();
        let current = current_announce
            .announce_list()
            .get_announce()
            .map(str::to_owned);
        let mut trackers = Vec::new();
        for tier in 0..current_announce.announce_list().tier_count() {
            let mut entry = 0;
            while let Some(uri) = current_announce
                .announce_list()
                .get_tracker_url(tier, entry)
            {
                trackers.push(aria2_rpc::TrackerInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: current.as_deref() == Some(uri.as_str()),
                    last_attempt: false,
                    announce_ready: announce.is_announce_ready(),
                    all_failed: announce.is_all_announce_failed(),
                    in_flight: current_announce.in_flight_announces(),
                    interval: announce.interval().as_secs(),
                    min_interval: announce.min_interval().as_secs(),
                    seeders: current_announce.complete(),
                    leechers: current_announce.incomplete(),
                    tracker_id: current_announce.tracker_id().to_string(),
                    seconds_since_last_success: current_announce.seconds_since_last_success(),
                });
                entry += 1;
            }
        }
        Ok(BackendResult::response(BackendResponse::Trackers(trackers)))
    }

    #[cfg(feature = "bittorrent")]
    pub(super) async fn get_dht_status(&self) -> Result<BackendResult, BackendError> {
        let engines = self
            .bt_registry
            .as_ref()
            .and_then(|registry| {
                registry
                    .read()
                    .ok()
                    .map(|registry| registry.get_dht_engines())
            })
            .unwrap_or_default();
        let mut stats = aria2_protocol::bittorrent::dht::engine::DhtEngineStats {
            total_nodes: 0,
            good_nodes: 0,
            pending_transactions: 0,
            state: aria2_protocol::bittorrent::dht::engine::DhtEngineState::Stopped,
        };
        for engine in engines {
            let current = engine.stats().await;
            stats.total_nodes += current.total_nodes;
            stats.good_nodes += current.good_nodes;
            stats.pending_transactions += current.pending_transactions;
            if dht_state_priority(current.state) > dht_state_priority(stats.state) {
                stats.state = current.state;
            }
        }
        Ok(BackendResult::response(BackendResponse::DhtStatus(
            aria2_rpc::DhtStatus {
                state: format!("{:?}", stats.state).to_lowercase(),
                total_nodes: stats.total_nodes,
                good_nodes: stats.good_nodes,
                pending_transactions: stats.pending_transactions,
            },
        )))
    }

    pub(super) fn get_uris(&self, gid: String) -> Result<BackendResult, BackendError> {
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

    pub(super) fn get_files(&self, gid: String) -> Result<BackendResult, BackendError> {
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

    pub(super) fn get_servers(&self, gid: String) -> Result<BackendResult, BackendError> {
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
}

#[cfg(feature = "bittorrent")]
fn tracker_infos_from_runtime(snapshot: &TrackerRuntimeSnapshot) -> Vec<aria2_rpc::TrackerInfo> {
    snapshot
        .tracker_tiers
        .iter()
        .enumerate()
        .flat_map(|(tier, uris)| {
            uris.iter().map(move |uri| aria2_rpc::TrackerInfo {
                uri: uri.clone(),
                tier: tier + 1,
                current: snapshot.current_url.as_deref() == Some(uri.as_str()),
                last_attempt: snapshot.last_attempt_url.as_deref() == Some(uri.as_str()),
                announce_ready: snapshot.announce_ready,
                all_failed: snapshot.all_failed,
                in_flight: snapshot.in_flight,
                interval: snapshot.interval_secs,
                min_interval: snapshot.min_interval_secs,
                seeders: snapshot.seeders,
                leechers: snapshot.leechers,
                tracker_id: snapshot.tracker_id.clone(),
                seconds_since_last_success: snapshot.seconds_since_last_success,
            })
        })
        .collect()
}

#[cfg(feature = "bittorrent")]
fn dht_state_priority(state: aria2_protocol::bittorrent::dht::engine::DhtEngineState) -> u8 {
    use aria2_protocol::bittorrent::dht::engine::DhtEngineState;
    match state {
        DhtEngineState::Stopped => 1,
        DhtEngineState::ShuttingDown => 2,
        DhtEngineState::Bootstrapping => 3,
        DhtEngineState::Running => 4,
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

pub(super) fn paginate<T>(items: Vec<T>, offset: i64, num: usize) -> Vec<T> {
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
                    .and_then(|uri| {
                        uri.rsplit('/')
                            .next()
                            .map(|name| name.split(['?', '#']).next().unwrap_or(name))
                            .map(str::to_owned)
                    })
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
