//! Pure RPC integration fixtures.
//!
//! These tests deliberately use an in-memory [`RpcBackend`]. The real core
//! adapter is owned by the `aria2` application crate; keeping it out of this
//! fixture makes the dependency direction testable at compile time.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use aria2_rpc::backend::{
    BackendError, BackendEvent, BackendMetadata, BackendReadSnapshot, BackendRequest,
    BackendResponse, BackendResult, PositionMode, RpcBackend,
};
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::server::{RpcAuthMiddleware, RpcServer, ServerConfig};
use aria2_rpc::types::{
    DownloadStatus, FileInfo, GlobalStat, ServerInfoIndex, StatusInfo, UriEntry, create_gid,
};
use async_trait::async_trait;
use serde_json::Value;

fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Clone)]
struct FakeTask {
    gid: String,
    uris: Vec<String>,
    options: HashMap<String, Value>,
    status: DownloadStatus,
}

struct FakeState {
    global_options: HashMap<String, Value>,
    tasks: HashMap<String, FakeTask>,
    order: Vec<String>,
    stopped: Vec<FakeTask>,
}

struct FakeBackend {
    state: Mutex<FakeState>,
    active_by_default: bool,
    save_session_path: Option<PathBuf>,
    metadata: BackendMetadata,
}

impl FakeBackend {
    fn new(max_concurrent: u32, save_session_path: Option<PathBuf>) -> Self {
        let metadata = BackendMetadata::base(env!("CARGO_PKG_VERSION"));
        #[cfg(feature = "bittorrent")]
        let metadata = metadata.with_bittorrent();
        #[cfg(feature = "metalink")]
        let metadata = metadata.with_metalink();
        #[cfg(feature = "sftp")]
        let metadata = metadata.with_sftp();

        let mut global_options = HashMap::new();
        for (name, value) in [
            ("dir", Value::String(".".into())),
            (
                "max-concurrent-downloads",
                Value::String(max_concurrent.to_string()),
            ),
            ("max-connection-per-server", Value::String("16".into())),
            ("max-download-limit", Value::String("0".into())),
            ("max-overall-download-limit", Value::String("0".into())),
            ("max-overall-upload-limit", Value::String("0".into())),
            ("max-upload-limit", Value::String("0".into())),
            ("no-conf", Value::String("false".into())),
            ("uri-selector", Value::String("feedback".into())),
        ] {
            global_options.insert(name.to_string(), value);
        }
        if let Some(path) = &save_session_path {
            global_options.insert(
                "save-session".into(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }

        Self {
            state: Mutex::new(FakeState {
                global_options,
                tasks: HashMap::new(),
                order: Vec::new(),
                stopped: Vec::new(),
            }),
            active_by_default: max_concurrent != 0,
            save_session_path,
            metadata,
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, FakeState>, BackendError> {
        self.state
            .lock()
            .map_err(|_| BackendError::Internal("RPC test backend state is poisoned".into()))
    }

    fn execution(message: impl Into<String>) -> BackendError {
        BackendError::Execution(message.into())
    }

    fn option_text(value: &Value) -> Option<String> {
        match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            Value::Bool(value) => Some(value.to_string()),
            Value::Array(values) => values
                .iter()
                .map(Self::option_text)
                .collect::<Option<Vec<_>>>()
                .map(|values| values.join("\n")),
            Value::Null | Value::Object(_) => None,
        }
    }

    fn integer_option(value: &Value, name: &str) -> Result<u64, BackendError> {
        Self::option_text(value)
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| Self::execution(format!("Option '{name}' must be an integer")))
    }

    fn validate_rate(value: &Value, name: &str) -> Result<(), BackendError> {
        let raw = Self::option_text(value)
            .ok_or_else(|| Self::execution(format!("Option '{name}' must be a byte rate")))?;
        let number = raw
            .trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G', 't', 'T'])
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0);
        if number.is_none() {
            return Err(Self::execution(format!(
                "Option '{name}' must be a byte rate"
            )));
        }
        Ok(())
    }

    fn validate_global_option(name: &str, value: &Value) -> Result<(), BackendError> {
        match name {
            "max-concurrent-downloads" | "max-connection-per-server" => {
                let _ = Self::integer_option(value, name)?;
            }
            "max-overall-download-limit" | "max-overall-upload-limit" => {
                Self::validate_rate(value, name)?;
            }
            "uri-selector" => {
                let value = Self::option_text(value).unwrap_or_default();
                if !matches!(value.as_str(), "feedback" | "inorder" | "parallel") {
                    return Err(Self::execution(format!(
                        "Option '{name}' has an invalid value"
                    )));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn task_path(task: &FakeTask) -> String {
        let name = task
            .options
            .get("out")
            .and_then(Self::option_text)
            .or_else(|| {
                task.uris
                    .first()
                    .and_then(|uri| uri.rsplit('/').next())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        match task.options.get("dir").and_then(Value::as_str) {
            Some(dir) if !dir.is_empty() && !name.is_empty() => {
                PathBuf::from(dir).join(name).to_string_lossy().into_owned()
            }
            _ => name,
        }
    }

    fn status_for_task(task: &FakeTask) -> StatusInfo {
        let file = FileInfo::new(Self::task_path(task), 0)
            .with_index(1)
            .with_completed(0)
            .with_uris(task.uris.iter().cloned().map(UriEntry::new).collect());
        StatusInfo::new(task.gid.clone())
            .with_status(task.status.clone())
            .with_total_length(0)
            .with_completed_length(0)
            .with_upload_length(0)
            .with_download_speed(0)
            .with_upload_speed(0)
            .with_connections(16)
            .with_dir(
                task.options
                    .get("dir")
                    .and_then(Value::as_str)
                    .unwrap_or("."),
            )
            .with_files(vec![file])
    }

    fn statuses_for<'a, I>(tasks: I) -> Vec<StatusInfo>
    where
        I: IntoIterator<Item = &'a FakeTask>,
    {
        tasks.into_iter().map(Self::status_for_task).collect()
    }

    fn stopped_statuses(state: &FakeState) -> Vec<StatusInfo> {
        Self::statuses_for(state.stopped.iter())
    }

    fn global_stat(state: &FakeState) -> GlobalStat {
        let active = state
            .tasks
            .values()
            .filter(|task| task.status == DownloadStatus::Active)
            .count();
        let waiting = state
            .tasks
            .values()
            .filter(|task| task.status == DownloadStatus::Waiting)
            .count();
        GlobalStat {
            download_speed: 0,
            upload_speed: 0,
            num_active: active,
            num_waiting: waiting,
            num_stopped: state.stopped.len(),
            num_stopped_total: state.stopped.len(),
        }
    }

    fn paginate<T>(items: Vec<T>, offset: i64, num: usize) -> Vec<T> {
        if num == 0 {
            return Vec::new();
        }
        let start = if offset < 0 {
            let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
            len.saturating_add(offset)
        } else {
            offset
        };
        if start < 0 || start as usize >= items.len() {
            return Vec::new();
        }
        items.into_iter().skip(start as usize).take(num).collect()
    }

    fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, Value>,
    ) -> Result<String, BackendError> {
        let gid = create_gid();
        let mut task_options = self.lock_state()?.global_options.clone();
        task_options.extend(options);
        let task = FakeTask {
            gid: gid.clone(),
            uris,
            options: task_options,
            status: if self.active_by_default {
                DownloadStatus::Active
            } else {
                DownloadStatus::Waiting
            },
        };
        let mut state = self.lock_state()?;
        state.order.push(gid.clone());
        state.tasks.insert(gid.clone(), task);
        Ok(gid)
    }

    fn remove_task(&self, gid: String) -> Result<BackendResult, BackendError> {
        let mut state = self.lock_state()?;
        let task = state
            .tasks
            .remove(&gid)
            .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
        state.order.retain(|candidate| candidate != &gid);
        let mut stopped = task;
        stopped.status = DownloadStatus::Removed;
        state.stopped.push(stopped);
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid.clone()),
            vec![BackendEvent::DownloadStop(gid)],
        ))
    }

    fn change_position(
        &self,
        gid: String,
        position: i32,
        mode: PositionMode,
    ) -> Result<BackendResult, BackendError> {
        let mut state = self.lock_state()?;
        let old_index = state
            .order
            .iter()
            .position(|candidate| candidate == &gid)
            .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
        state.order.remove(old_index);
        let len = state.order.len();
        let target = match mode {
            PositionMode::SetFromStart => position.max(0) as usize,
            PositionMode::MoveFromStart => (old_index as i32 + position).max(0) as usize,
            PositionMode::SetFromEnd => {
                let from_end = position.max(0) as usize;
                len.saturating_sub(from_end.saturating_add(1))
            }
        }
        .min(len);
        state.order.insert(target, gid);
        Ok(BackendResult::response(BackendResponse::Position(target)))
    }

    fn task_options(options: HashMap<String, Value>) -> HashMap<String, Value> {
        options
            .into_iter()
            .filter_map(|(key, value)| {
                Self::option_text(&value).map(|value| (key, Value::String(value)))
            })
            .collect()
    }
}

#[async_trait]
impl RpcBackend for FakeBackend {
    fn metadata(&self) -> BackendMetadata {
        self.metadata.clone()
    }

    async fn task_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.tasks.len())
            .unwrap_or(0)
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        match request {
            BackendRequest::AddUri {
                uris,
                options,
                position,
            } => {
                for (name, value) in &options {
                    if name == "metalink-preferred-protocol"
                        && Self::option_text(value).as_deref() == Some("gopher")
                    {
                        return Err(Self::execution(format!(
                            "Option '{name}' has an invalid value"
                        )));
                    }
                }
                let gid = self.add_task(uris, Self::task_options(options))?;
                if let Some(position) = position {
                    self.change_position(gid.clone(), position as i32, PositionMode::SetFromStart)?;
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(gid.clone()),
                    vec![BackendEvent::DownloadStart(gid)],
                ))
            }
            BackendRequest::AddTorrent {
                additional_uris,
                options,
                position,
                ..
            } => {
                let gid = self.add_task(additional_uris, Self::task_options(options))?;
                if let Some(position) = position {
                    self.change_position(gid.clone(), position as i32, PositionMode::SetFromStart)?;
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(gid.clone()),
                    vec![BackendEvent::DownloadStart(gid)],
                ))
            }
            BackendRequest::AddMetalink {
                options, position, ..
            } => {
                let gid = self.add_task(Vec::new(), Self::task_options(options))?;
                if let Some(position) = position {
                    self.change_position(gid.clone(), position as i32, PositionMode::SetFromStart)?;
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gids(vec![gid.clone()]),
                    vec![BackendEvent::DownloadStart(gid)],
                ))
            }
            BackendRequest::Remove { gid } => self.remove_task(gid),
            BackendRequest::ForceRemove { gids } => {
                let mut events = Vec::new();
                let mut last = String::new();
                for gid in gids {
                    last = gid.clone();
                    events.extend(self.remove_task(gid)?.events);
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(last),
                    events,
                ))
            }
            BackendRequest::Pause { gid } | BackendRequest::ForcePause { gid } => {
                let mut state = self.lock_state()?;
                let task = state
                    .tasks
                    .get_mut(&gid)
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                if task.status == DownloadStatus::Paused {
                    return Err(Self::execution(format!("GID {gid} is already paused")));
                }
                task.status = DownloadStatus::Paused;
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(gid.clone()),
                    vec![BackendEvent::DownloadPause(gid)],
                ))
            }
            BackendRequest::Unpause { gid } => {
                let mut state = self.lock_state()?;
                let task = state
                    .tasks
                    .get_mut(&gid)
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                if task.status != DownloadStatus::Paused {
                    return Err(Self::execution(format!("GID {gid} is not paused")));
                }
                task.status = if self.active_by_default {
                    DownloadStatus::Active
                } else {
                    DownloadStatus::Waiting
                };
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(gid.clone()),
                    vec![BackendEvent::DownloadStart(gid)],
                ))
            }
            BackendRequest::TellStatus { gid, .. } => {
                let state = self.lock_state()?;
                if let Some(task) = state.tasks.get(&gid) {
                    return Ok(BackendResult::response(BackendResponse::Status(
                        Self::status_for_task(task),
                    )));
                }
                if let Some(task) = state.stopped.iter().find(|task| task.gid == gid) {
                    return Ok(BackendResult::response(BackendResponse::Status(
                        Self::status_for_task(task),
                    )));
                }
                Err(Self::execution(format!("GID {gid} not found")))
            }
            BackendRequest::TellActive { .. } => {
                let state = self.lock_state()?;
                let statuses = Self::statuses_for(
                    state
                        .tasks
                        .values()
                        .filter(|task| task.status == DownloadStatus::Active),
                );
                Ok(BackendResult::response(BackendResponse::Statuses(statuses)))
            }
            BackendRequest::TellWaiting { offset, num, .. } => {
                let state = self.lock_state()?;
                let statuses = state
                    .order
                    .iter()
                    .filter_map(|gid| state.tasks.get(gid))
                    .filter(|task| task.status == DownloadStatus::Waiting)
                    .map(Self::status_for_task)
                    .collect();
                Ok(BackendResult::response(BackendResponse::Statuses(
                    Self::paginate(statuses, offset, num),
                )))
            }
            BackendRequest::TellStopped { offset, num, .. } => {
                let state = self.lock_state()?;
                Ok(BackendResult::response(BackendResponse::Statuses(
                    Self::paginate(Self::stopped_statuses(&state), offset, num),
                )))
            }
            BackendRequest::GetGlobalStat => {
                let state = self.lock_state()?;
                Ok(BackendResult::response(BackendResponse::GlobalStat(
                    Self::global_stat(&state),
                )))
            }
            BackendRequest::GetUris { gid } => {
                let state = self.lock_state()?;
                let task = state
                    .tasks
                    .get(&gid)
                    .or_else(|| state.stopped.iter().find(|task| task.gid == gid))
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                Ok(BackendResult::response(BackendResponse::Uris(
                    task.uris.iter().cloned().map(UriEntry::new).collect(),
                )))
            }
            BackendRequest::GetFiles { gid } => {
                let state = self.lock_state()?;
                let task = state
                    .tasks
                    .get(&gid)
                    .or_else(|| state.stopped.iter().find(|task| task.gid == gid))
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                let file = FileInfo::new(Self::task_path(task), 0)
                    .with_index(1)
                    .with_completed(0)
                    .with_uris(task.uris.iter().cloned().map(UriEntry::new).collect());
                Ok(BackendResult::response(BackendResponse::Files(vec![file])))
            }
            BackendRequest::GetServers { gid } => {
                let state = self.lock_state()?;
                let task = state
                    .tasks
                    .get(&gid)
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                if task.status != DownloadStatus::Active {
                    return Err(Self::execution(format!("No active download for GID#{gid}")));
                }
                Ok(BackendResult::response(BackendResponse::Servers(vec![
                    ServerInfoIndex {
                        index: 1,
                        servers: Vec::new(),
                    },
                ])))
            }
            BackendRequest::PurgeDownloadResult => {
                self.lock_state()?.stopped.clear();
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::RemoveDownloadResult { gid } => {
                let mut state = self.lock_state()?;
                let before = state.stopped.len();
                state.stopped.retain(|task| task.gid != gid);
                if state.stopped.len() == before {
                    return Err(Self::execution(format!(
                        "GID {gid} not found in download results"
                    )));
                }
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::GetGlobalOption => Ok(BackendResult::response(
                BackendResponse::Options(self.lock_state()?.global_options.clone()),
            )),
            BackendRequest::ChangeGlobalOption { options } => {
                let mut state = self.lock_state()?;
                for (name, value) in options {
                    if name == "no-conf" {
                        continue;
                    }
                    if !matches!(
                        name.as_str(),
                        "dir"
                            | "save-session"
                            | "max-concurrent-downloads"
                            | "max-connection-per-server"
                            | "max-download-limit"
                            | "max-overall-download-limit"
                            | "max-overall-upload-limit"
                            | "max-upload-limit"
                            | "uri-selector"
                    ) {
                        continue;
                    }
                    Self::validate_global_option(&name, &value)?;
                    if let Some(value) = Self::option_text(&value) {
                        state.global_options.insert(name, Value::String(value));
                    }
                }
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::GetOption { gid } => {
                let state = self.lock_state()?;
                let task = state
                    .tasks
                    .get(&gid)
                    .or_else(|| state.stopped.iter().find(|task| task.gid == gid))
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                Ok(BackendResult::response(BackendResponse::Options(
                    task.options.clone(),
                )))
            }
            BackendRequest::ChangeOption { gid, options } => {
                let mut state = self.lock_state()?;
                let task = state
                    .tasks
                    .get_mut(&gid)
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                for (name, value) in options {
                    if name == "max-download-limit" || name == "max-upload-limit" {
                        Self::validate_rate(&value, &name)?;
                        if let Some(value) = Self::option_text(&value) {
                            task.options.insert(name, Value::String(value));
                        }
                    } else if matches!(
                        name.as_str(),
                        "bt-max-peers"
                            | "bt-remove-unselected-file"
                            | "bt-request-peer-speed-limit"
                            | "force-save"
                            | "save-not-found"
                            | "allow-overwrite"
                    ) && let Some(value) = Self::option_text(&value)
                    {
                        task.options.insert(name, Value::String(value));
                    }
                }
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::GetPeers { gid } => {
                let state = self.lock_state()?;
                if !state.tasks.contains_key(&gid)
                    && !state.stopped.iter().any(|task| task.gid == gid)
                {
                    return Err(Self::execution(format!("GID {gid} not found")));
                }
                Ok(BackendResult::response(BackendResponse::Peers(Vec::new())))
            }
            BackendRequest::PauseAll | BackendRequest::ForcePauseAll => {
                let mut state = self.lock_state()?;
                let mut events = Vec::new();
                for task in state.tasks.values_mut() {
                    if task.status != DownloadStatus::Paused {
                        task.status = DownloadStatus::Paused;
                        events.push(BackendEvent::DownloadPause(task.gid.clone()));
                    }
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    events,
                ))
            }
            BackendRequest::UnpauseAll => {
                let mut state = self.lock_state()?;
                let mut events = Vec::new();
                for task in state.tasks.values_mut() {
                    if task.status == DownloadStatus::Paused {
                        task.status = if self.active_by_default {
                            DownloadStatus::Active
                        } else {
                            DownloadStatus::Waiting
                        };
                        events.push(BackendEvent::DownloadStart(task.gid.clone()));
                    }
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    events,
                ))
            }
            BackendRequest::ChangeUri {
                gid,
                file_index,
                delete_uris,
                add_uris,
                position,
            } => {
                if file_index != 1 {
                    return Err(Self::execution("fileIndex must be 1"));
                }
                let mut state = self.lock_state()?;
                let task = state
                    .tasks
                    .get_mut(&gid)
                    .ok_or_else(|| Self::execution(format!("GID {gid} not found")))?;
                let before = task.uris.len();
                task.uris.retain(|uri| !delete_uris.contains(uri));
                let deleted = before - task.uris.len();
                let added = add_uris.len();
                let insertion = position.unwrap_or(task.uris.len()).min(task.uris.len());
                task.uris.splice(insertion..insertion, add_uris);
                Ok(BackendResult::response(BackendResponse::Counts([
                    deleted, added,
                ])))
            }
            BackendRequest::SaveSession => {
                let path = self
                    .save_session_path
                    .as_ref()
                    .ok_or_else(|| Self::execution("Filename is not given. Set --save-session."))?
                    .clone();
                let contents = {
                    let state = self.lock_state()?;
                    state
                        .order
                        .iter()
                        .filter_map(|gid| state.tasks.get(gid))
                        .map(|task| format!("{}\n", task.uris.join(" ")))
                        .collect::<String>()
                };
                tokio::fs::write(path, contents).await.map_err(|error| {
                    BackendError::Internal(format!("Failed to save session: {error}"))
                })?;
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::ChangePosition {
                gid,
                position,
                mode,
            } => self.change_position(gid, position, mode),
            BackendRequest::Shutdown { force } => {
                let mut state = self.lock_state()?;
                let count = state.tasks.len();
                if force {
                    state.tasks.clear();
                    state.order.clear();
                }
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
        let state = self.lock_state()?;
        let active = Self::statuses_for(
            state
                .tasks
                .values()
                .filter(|task| task.status == DownloadStatus::Active),
        );
        let waiting = state
            .order
            .iter()
            .filter_map(|gid| state.tasks.get(gid))
            .filter(|task| task.status == DownloadStatus::Waiting)
            .map(Self::status_for_task)
            .collect();
        let stopped = Self::stopped_statuses(&state);
        Ok(Some(Arc::new(BackendReadSnapshot {
            active,
            waiting,
            global_stat: Self::global_stat(&state),
            stopped,
        })))
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
                    Self::paginate(snapshot.waiting.clone(), *offset, *num),
                )))
            }
            (BackendRequest::TellStopped { offset, num, .. }, Some(snapshot)) => {
                Ok(BackendResult::response(BackendResponse::Statuses(
                    Self::paginate(snapshot.stopped.clone(), *offset, *num),
                )))
            }
            (BackendRequest::GetGlobalStat, Some(snapshot)) => Ok(BackendResult::response(
                BackendResponse::GlobalStat(snapshot.global_stat.clone()),
            )),
            _ => self.execute(request).await,
        }
    }
}

pub fn test_engine() -> RpcEngine {
    test_engine_with_max_concurrent(5)
}

pub fn test_engine_with_max_concurrent(max_concurrent: u32) -> RpcEngine {
    RpcEngine::with_backend(Arc::new(FakeBackend::new(max_concurrent, None)))
}

pub async fn start_test_server(token: Option<&str>) -> (String, TestServerHandle) {
    start_test_server_with_max_concurrent(token, 5).await
}

pub async fn start_test_server_with_max_concurrent(
    token: Option<&str>,
    max_concurrent: u32,
) -> (String, TestServerHandle) {
    start_test_server_with_config(token, max_concurrent, ServerConfig::default()).await
}

pub async fn start_test_server_with_config(
    token: Option<&str>,
    max_concurrent: u32,
    config: ServerConfig,
) -> (String, TestServerHandle) {
    ensure_crypto_provider();
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("Failed to bind to random port");
    let port = listener
        .local_addr()
        .expect("Failed to read random listener address")
        .port();
    drop(listener);

    let config = config.with_host("127.0.0.1").with_port(port);
    let save_session_path = std::env::temp_dir().join(format!(
        "aria2_rpc_e2e_{}_{}.sess",
        std::process::id(),
        create_gid()
    ));
    let backend = Arc::new(FakeBackend::new(max_concurrent, Some(save_session_path)));
    let rpc_engine = RpcEngine::with_backend(backend);
    let rpc_engine = if let Some(token) = token {
        rpc_engine.with_auth_middleware(RpcAuthMiddleware::new(token))
    } else {
        rpc_engine
    };
    let server = RpcServer::new_with_engine(config, Arc::new(rpc_engine))
        .expect("Failed to create RpcServer");
    let base_url = format!("http://127.0.0.1:{port}");
    let server_task = tokio::spawn(async move {
        if let Err(error) = server.serve().await {
            eprintln!("[test-helper] RPC server exited with error: {error}");
        }
    });
    wait_for_server_ready(&base_url).await;
    (
        base_url,
        TestServerHandle {
            server_task: Some(server_task),
        },
    )
}

async fn wait_for_server_ready(base_url: &str) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_error = String::new();
    while tokio::time::Instant::now() < deadline {
        match client.get(format!("{base_url}/jsonrpc")).send().await {
            Ok(response) if response.status() != reqwest::StatusCode::NOT_FOUND => return,
            Ok(response) => last_error = format!("status={}", response.status()),
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Server at {base_url} did not become ready within 5 s (last error: {last_error})");
}

pub struct TestServerHandle {
    server_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
    }
}
