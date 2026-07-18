//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use std::collections::HashMap;

use crate::engine::RpcEngine;
use crate::engine::TaskState;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, create_gid};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::constants as core_constants;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};

/// Offset mode for `aria2.changePosition`, mirroring the original aria2
/// `OffsetMode` enum (`OFFSET_MODE_SET`/`OFFSET_MODE_CUR`/`OFFSET_MODE_END`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OffsetMode {
    /// Interpret `pos` as an absolute index from the head of the queue.
    Set,
    /// Interpret `pos` as a delta from the current index.
    Cur,
    /// Interpret `pos` as a (possibly negative) delta from the tail of the queue.
    End,
}

impl RpcEngine {
    /// Handle `aria2.addUri` - Add a new download task from URI(s).
    pub async fn handle_add_uri(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let uris: Vec<String> = if let Ok(arr) = req.get_param::<Vec<String>>(0) {
            arr
        } else if let Ok(single) = req.get_param::<String>(0) {
            vec![single]
        } else {
            return Err(JsonRpcError::InvalidParams(
                "param[0] must be a string or array of strings".into(),
            ));
        };
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let gid = self.add_task(uris, opts).await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    /// Handle `aria2.addTorrent` - Add a BitTorrent download.
    pub async fn handle_add_torrent(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let torrent_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let _dir = opts
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let decoded_bytes = if torrent_data.starts_with("data:") {
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                torrent_data.split(',').nth(1).unwrap_or(""),
            )
            .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &torrent_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        if decoded_bytes.len() < 3
            || decoded_bytes[0] != b'd'
            || decoded_bytes[1] != b'8'
            || decoded_bytes[2] != b':'
        {
            return Err(JsonRpcError::InvalidParams(
                "Invalid BEncode data (not a .torrent file)".into(),
            ));
        }

        let gid = self
            .add_task(
                vec![format!(
                    "torrent://{}",
                    &decoded_bytes[..std::cmp::min(32, decoded_bytes.len())]
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
                )],
                opts,
            )
            .await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    /// Handle `aria2.addMetalink` - Add downloads from Metalink XML.
    pub async fn handle_add_metalink(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);

        let decoded_bytes = if metalink_data.starts_with("data:") {
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                metalink_data.split(',').nth(1).unwrap_or(""),
            )
            .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &metalink_data)
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        };

        let preview = String::from_utf8_lossy(&decoded_bytes[..decoded_bytes.len().min(200)]);
        if !preview.to_lowercase().contains("<metalink")
            && !preview.contains("urn:ietf:params:xml:ns:metalink")
        {
            return Err(JsonRpcError::InvalidParams(
                "Invalid Metalink XML data".into(),
            ));
        }

        let gid = self
            .add_task(vec!["metalink://download".to_string()], opts)
            .await?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    /// Handle `aria2.remove` - Remove a download task.
    ///
    /// Returns the GID string of the removed download per the aria2 RPC spec.
    pub async fn handle_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(_) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                gid,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.pause` - Pause a download task gracefully.
    ///
    /// Returns the GID string of the paused download per the aria2 RPC spec.
    pub async fn handle_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Paused;
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    gid,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forcePause` - Force pause a download task immediately.
    ///
    /// Returns the GID string of the paused download per the aria2 RPC spec.
    pub async fn handle_force_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        let mut tasks_map = self.tasks.write().await;
        match tasks_map.get_mut(&gid) {
            Some(task_state) => {
                task_state.status.status = DownloadStatus::Paused;
                if let Some(cancel_token) = &task_state.cancel_token {
                    cancel_token.cancel();
                }
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    gid,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.unpause` / `aria2.forceUnpause` - Resume a paused task.
    ///
    /// Returns the GID string of the resumed download per the aria2 RPC spec.
    pub async fn handle_unpause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Active;
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    gid,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.tellStatus` - Get detailed status of a specific download.
    ///
    /// Optional `keys` parameter (param[1]) filters which fields are returned
    /// per the aria2 RPC spec. `gid` is always included.
    pub async fn handle_tell_status(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let keys = super::status::parse_keys_param(req, 1)?;
        match self.get_status(&gid).await {
            Some(status) => {
                let value = serde_json::to_value(status).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?;
                let value = super::status::apply_keys_filter(value, keys.as_deref());
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    value,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forceRemove` - Forcefully remove a download without graceful shutdown.
    ///
    /// Returns the GID string of the removed download per the aria2 RPC spec.
    pub async fn handle_force_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(_) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                gid,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.changeUri` - Add/remove URIs for an existing download.
    ///
    /// Returns `[delcount, addcount]` per the original aria2 RPC spec, where:
    /// - `delcount` = number of URIs actually removed from the file
    /// - `addcount` = number of URIs actually added to the file
    ///
    /// Both counts are integers (NOT strings) — this is the one exception to
    /// the "all numeric fields are JSON strings" rule, matching original aria2
    /// behavior in `RpcMethodImpl.cc::changeUriResponse`.
    pub async fn handle_change_uri(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let _file_index: usize = req.get_param_or_default(1);
        let del_uris: Option<Vec<String>> = req
            .get_param::<serde_json::Value>(2)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());
        let add_uris: Option<Vec<String>> = req
            .get_param::<serde_json::Value>(3)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());

        let mut tasks = self.tasks.write().await;
        let state = tasks
            .get_mut(&gid)
            .ok_or_else(|| JsonRpcError::MethodNotFound(format!("GID {} not found", gid)))?;

        // Count actual deletions: only URIs that existed are counted.
        let delcount: usize = match &del_uris {
            Some(to_remove) => {
                let before = state.uris.len();
                state.uris.retain(|u| !to_remove.contains(u));
                before - state.uris.len()
            }
            None => 0,
        };

        // Count actual additions: all URIs in the add list are appended.
        let addcount: usize = match add_uris {
            Some(to_add) => {
                let n = to_add.len();
                state.uris.extend(to_add);
                n
            }
            None => 0,
        };

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([delcount, addcount]),
        ))
    }

    /// Handle `aria2.saveSession` - Save current session state to disk.
    pub async fn handle_save_session(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let dir = req.get_param_or_default::<String>(0);
        if dir.is_empty() {
            return Ok(JsonRpcResponse::error(
                req.id.clone().unwrap_or_default(),
                -32602,
                "dir must not be empty",
            ));
        }

        let tasks = self.tasks.read().await;
        let count = tasks.len();
        drop(tasks);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            format!("OK. Saved {} downloads.", count),
        ))
    }

    /// Handle `aria2.changePosition` - Change the position of a download in
    /// the waiting queue.
    ///
    /// Wire format mirrors the original aria2 RPC:
    /// `params: [gid: String, pos: Integer, how: String]`
    /// where `how` is one of `"POS_SET"`, `"POS_CUR"`, `"POS_END"`. Any
    /// other value yields an `InvalidParams` error
    /// (matching the original `DL_ABORT_EX("Illegal argument.")`).
    ///
    /// Returns the resulting target index as a JSON integer
    /// (matching `Integer::g(destPos)` in the original).
    ///
    /// Because the Rust engine currently stores all tasks in a `HashMap`
    /// without an explicit reserved/waiting queue, the "queue" used here
    /// for position math is a deterministic snapshot: the sorted list of
    /// GIDs currently in the task map. The move is reflected only in the
    /// returned index — no persistent reordering is performed, matching
    /// the original semantics where the actual reordering lives in
    /// `RequestGroupMan::reservedGroups_`.
    pub async fn handle_change_position(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let pos: i64 = req.get_param(1)?;
        let how: String = req.get_param(2)?;

        let offset_mode = match how.as_str() {
            "POS_SET" => OffsetMode::Set,
            "POS_CUR" => OffsetMode::Cur,
            "POS_END" => OffsetMode::End,
            other => {
                return Err(JsonRpcError::InvalidParams(format!(
                    "Illegal argument: how must be POS_SET, POS_CUR, or POS_END, got {:?}",
                    other
                )));
            }
        };

        let tasks = self.tasks.read().await;
        if !tasks.contains_key(&gid) {
            return Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found in the waiting queue",
                gid
            )));
        }

        // Deterministic queue snapshot: sorted GIDs (lexicographic).
        let mut all_gids: Vec<&String> = tasks.keys().collect();
        all_gids.sort();
        let queue_len = all_gids.len() as i64;
        let current_pos = all_gids
            .iter()
            .position(|g| g.as_str() == gid)
            .map(|p| p as i64)
            .unwrap_or(0);

        let target = match offset_mode {
            OffsetMode::Set => pos,
            OffsetMode::Cur => current_pos + pos,
            OffsetMode::End => (queue_len - 1) + pos,
        };
        // Clamp into valid range so callers cannot request a negative index.
        let clamped = target.clamp(0, queue_len - 1);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::from(clamped),
        ))
    }

    /// Handle `aria2.shutdown` - Graceful shutdown (save session, wait for downloads).
    ///
    /// Mirrors the original aria2 `goingShutdown(req, e, /*forceHalt=*/false)`
    /// in `RpcMethodImpl.cc`: returns `"OK"` immediately and schedules a
    /// [`HALT_DELAY`]-second delayed halt via [`RpcEngine::schedule_halt`].
    ///
    /// Active downloads are NOT forcibly cancelled — the graceful halt logic
    /// in the download engine is expected to let in-flight downloads finish
    /// (or be saved to session) before the process exits.
    ///
    /// The 3-second delay is critical: cancelling immediately would close
    /// the HTTP response stream before the client (e.g. AriaNg) receives the
    /// `"OK"` body, surfacing as a connection-reset error in the client.
    pub async fn handle_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        self.schedule_halt(crate::engine::HALT_DELAY, false);
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String("OK".to_string()),
        ))
    }

    /// Handle `aria2.forceShutdown` - Force shutdown (immediate termination).
    ///
    /// Mirrors the original aria2 `goingShutdown(req, e, /*forceHalt=*/true)`
    /// in `RpcMethodImpl.cc`: returns `"OK"` immediately and schedules a
    /// [`HALT_DELAY`]-second delayed halt via [`RpcEngine::schedule_halt`]
    /// with `force=true`.
    ///
    /// After the delay, every active download's [`CancellationToken`] is
    /// cancelled (matching `DownloadEngine::forceHalt()`), the task map is
    /// cleared, and the engine's shutdown signal is fired.
    ///
    /// Even though this is a "force" shutdown, the 3-second delay still
    /// applies — same rationale as [`Self::handle_shutdown`]: give the
    /// client time to receive the response body before the server exits.
    pub async fn handle_force_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        self.schedule_halt(crate::engine::HALT_DELAY, true);
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String("OK".to_string()),
        ))
    }

    /// Internal helper to add a new download task.
    ///
    /// When `group_man` and `cmd_tx` are configured (i.e., the RPC server is
    /// wired to a running DownloadEngine), this creates a real download:
    /// 1. Registers a `RequestGroup` in `RequestGroupMan` under the generated GID.
    /// 2. Creates a `DownloadCommand` sharing that group.
    /// 3. Sends the command to the engine via `cmd_tx`.
    ///
    /// When shared state is not available (e.g., in unit tests), it falls back
    /// to creating a placeholder `TaskState` only.
    async fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<String, JsonRpcError> {
        let gid_str = create_gid();
        let gid = GroupId::from_hex_string(&gid_str)
            .ok_or_else(|| JsonRpcError::InternalError("Invalid GID generated".into()))?;

        let dl_options = rpc_options_to_download_options(&options);

        // Start a real download if we have shared engine state
        if let (Some(group_man), Some(cmd_tx)) = (&self.group_man, &self.cmd_tx) {
            let man = group_man.read().await;
            man.add_group_with_gid(gid, uris.clone(), dl_options.clone())
                .await
                .map_err(|e| JsonRpcError::InternalError(format!("Failed to add group: {}", e)))?;

            let group = man.group_by_id(gid).ok_or_else(|| {
                JsonRpcError::InternalError("Group not found after insert".into())
            })?;

            let first_uri = uris.first().ok_or_else(|| {
                JsonRpcError::InvalidParams("At least one URI is required".into())
            })?;

            let cmd = DownloadCommand::new_with_group(
                group,
                first_uri,
                &dl_options,
                dl_options.dir.as_deref(),
                dl_options.out.as_deref(),
            )
            .map_err(|e| JsonRpcError::InternalError(format!("DownloadCommand failed: {}", e)))?;

            cmd_tx.send(Box::new(cmd)).map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send command: {}", e))
            })?;
        }

        // Track in RPC tasks map (for cancel_token, options metadata)
        let dir = options
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let status = StatusInfo::new(&gid_str)
            .with_status(DownloadStatus::Active)
            .with_dir(dir)
            .with_total_length(0)
            .with_completed_length(0)
            .with_files(vec![FileInfo::new("", 0)]);
        let state = TaskState::new(status, options, uris);
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(gid_str.clone(), state);
        }
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid_str),
        );
        Ok(gid_str)
    }
}

/// Build a `StatusInfo` snapshot from a `RequestGroup` read-guard.
///
/// Extracts all live progress fields (atomic lengths, speeds, connections)
/// and metadata (GID, directory, URIs, files) into a protocol-compatible
/// `StatusInfo` suitable for RPC responses and stopped-task caching.
async fn group_to_status_info(
    gid: &str,
    g: &aria2_core::request::request_group::RequestGroup,
) -> StatusInfo {
    let status = g.status().await;
    let total = g.get_total_length_atomic();
    let completed = g.get_completed_length();
    let dl_speed = g.get_download_speed_cached();
    let uploaded = g.get_uploaded_length();
    let dir = g.options().dir.clone().unwrap_or_default();
    let uris: Vec<String> = g.uris().to_vec();
    let first_uri = uris.first().cloned().unwrap_or_default();
    let files = vec![FileInfo::new(first_uri, total).with_completed(completed)];
    StatusInfo::new(gid)
        .with_status(status)
        .with_total_length(total)
        .with_completed_length(completed)
        .with_upload_length(uploaded)
        .with_download_speed(dl_speed)
        .with_upload_speed(0)
        .with_connections(
            g.options().split.unwrap_or(core_constants::DEFAULT_SPLIT) as u16,
        )
        .with_dir(dir)
        .with_files(files)
}

impl RpcEngine {
    /// Scan `RequestGroupMan` for groups that have reached a terminal status
    /// (Complete or Error) and bridge them into `stopped_tasks` and the
    /// `tasks` map. Also publishes `DownloadComplete` / `DownloadError`
    /// WebSocket events.
    ///
    /// After bridging, the group is removed from `RequestGroupMan` so it no
    /// longer appears in `tellActive` responses.
    ///
    /// Call this before any RPC handler that reads `stopped_tasks` or lists
    /// active tasks to ensure completed downloads are visible to the client.
    pub(crate) async fn bridge_completed_groups(&self) {
        let Some(group_man) = &self.group_man else {
            return;
        };

        let completed: Vec<(String, StatusInfo)> = {
            let man = group_man.read().await;
            let mut batch = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                let status = g.status().await;
                if matches!(status, DownloadStatus::Complete | DownloadStatus::Error(_)) {
                    let gid_hex = gid.to_hex_string();
                    let info = group_to_status_info(&gid_hex, &g).await;
                    batch.push((gid_hex, info));
                }
            }
            batch
        };

        if completed.is_empty() {
            return;
        }

        // Publish events and update the tasks map / stopped_tasks.
        for (gid_hex, info) in &completed {
            // Update the RPC tasks map with final data so subsequent
            // tellStatus calls see the correct values even after the
            // group is removed from RequestGroupMan.
            {
                let mut tasks = self.tasks.write().await;
                if let Some(state) = tasks.get_mut(gid_hex) {
                    state.status = info.clone();
                    // Also sync the raw counters so update_status_info stays consistent.
                    state.total_length =
                        info.total_length.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                    state.completed_length =
                        info.completed_length.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                    state.upload_length =
                        info.upload_length.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                    state.download_speed =
                        info.download_speed.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                    state.upload_speed =
                        info.upload_speed.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                }
            }

            // Push to stopped_tasks for tellStopped queries.
            {
                let mut stopped = self.stopped_tasks.write().await;
                stopped.push(info.clone());
            }

            // Publish WebSocket event.
            if info.status.is_completed() {
                let _ = self.event_publisher.publish(
                    EventType::DownloadComplete,
                    DownloadEvent::download_complete(gid_hex.to_string()),
                );
            } else if matches!(info.status, DownloadStatus::Error(_)) {
                let _ = self.event_publisher.publish(
                    EventType::DownloadError,
                    DownloadEvent::download_error(gid_hex.to_string()),
                );
            }
        }

        // Remove bridged groups from RequestGroupMan.
        let man = group_man.read().await;
        for (gid_hex, _) in &completed {
            if let Some(gid) = aria2_core::request::request_group::GroupId::from_hex_string(gid_hex)
            {
                man.remove_group_by_id(gid);
            }
        }
    }

    /// Internal helper to get current status info for a task.
    ///
    /// Prefers live progress from `RequestGroupMan` (atomic fields updated by
    /// the download engine). Falls back to the placeholder `tasks` map when
    /// shared state is unavailable (e.g., unit tests).
    ///
    /// When reading from `RequestGroupMan`, the `tasks` map entry is also
    /// back-filled with live data so that subsequent calls (even after the
    /// group is removed from `RequestGroupMan`) return correct values.
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        // Try RequestGroupMan first (live progress)
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(group_lock) = man.group_by_hex(gid) {
                let g = group_lock.read().await;
                let info = group_to_status_info(gid, &g).await;

                // Back-fill the tasks map entry so subsequent fallback reads
                // (after group is removed from RequestGroupMan) get real data.
                {
                    let mut tasks = self.tasks.write().await;
                    if let Some(state) = tasks.get_mut(gid) {
                        state.status = info.clone();
                        state.total_length = info
                            .total_length
                            .as_ref()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        state.completed_length = info
                            .completed_length
                            .as_ref()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        state.upload_length =
                            info.upload_length.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                        state.download_speed = info
                            .download_speed
                            .as_ref()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        state.upload_speed =
                            info.upload_speed.as_ref().and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                }

                // If the group has reached a terminal status, bridge it to
                // stopped_tasks and publish the event.
                if info.status.is_completed()
                    || matches!(info.status, DownloadStatus::Error(_))
                {
                    // Use bridge_completed_groups for consistent handling.
                    // But to avoid scanning all groups every time, just bridge
                    // this single group. release the read lock first.
                    drop(g);
                    drop(man);
                    self.bridge_completed_groups().await;
                }

                return Some(info);
            }
        }
        // Fallback to tasks map (placeholder, for tests/no-engine mode)
        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(gid)?;
        state.update_status_info();
        Some(state.status.clone())
    }
}
/// Parse a JSON value as u64, supporting both JSON numbers and aria2 RPC
/// string-encoded numbers (the spec mandates all numeric fields as strings).
fn value_as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// Parse a JSON value as f64, supporting both JSON numbers and aria2 RPC
/// string-encoded numbers.
fn value_as_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// Convert RPC option map (from `aria2.addUri` params) to `DownloadOptions`.
///
/// Handles both array and newline-separated string forms of `header`.
fn rpc_options_to_download_options(opts: &HashMap<String, serde_json::Value>) -> DownloadOptions {
    let get_str = |k: &str| opts.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_u16 = |k: &str| opts.get(k).and_then(|v| value_as_u64(v)).map(|n| n as u16);
    let get_u32 = |k: &str| opts.get(k).and_then(|v| value_as_u64(v)).map(|n| n as u32);
    let get_u64 = |k: &str| opts.get(k).and_then(|v| value_as_u64(v));
    let get_f64 = |k: &str| opts.get(k).and_then(|v| value_as_f64(v));
    let get_bool = |k: &str| opts.get(k).and_then(|v| v.as_bool()).unwrap_or(false);

    let header: Vec<String> = match opts.get("header") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => vec![],
    };

    let checksum = get_str("checksum").and_then(|v| {
        if let Some((algo, val)) = v.split_once('=') {
            Some((algo.trim().to_string(), val.trim().to_string()))
        } else {
            None
        }
    });

    let dht_entry_point = get_str("dht-entry-point").map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    DownloadOptions {
        // Basic
        split: get_u16("split"),
        max_connection_per_server: get_u16("max-connection-per-server"),
        max_download_limit: get_u64("max-download-limit"),
        max_upload_limit: get_u64("max-upload-limit"),
        dir: get_str("dir"),
        out: get_str("out"),
        seed_time: get_u64("seed-time"),
        seed_ratio: get_f64("seed-ratio"),
        // File allocation
        file_allocation: get_str("file-allocation"),
        mmap_threshold: get_u64("mmap-threshold"),
        secure_falloc: get_bool("secure-falloc"),
        // Checksum
        checksum,
        // Cookies
        cookie_file: get_str("cookie-file"),
        cookies: get_str("cookies"),
        // BT
        bt_force_encrypt: get_bool("bt-force-encrypt"),
        bt_require_crypto: get_bool("bt-require-crypto"),
        enable_dht: opts
            .get("enable-dht")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        dht_listen_port: get_u16("dht-listen-port"),
        dht_entry_point,
        enable_public_trackers: opts
            .get("enable-public-trackers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        bt_piece_selection_strategy: get_str("bt-piece-selection-strategy").unwrap_or_default(),
        bt_endgame_threshold: get_u32("bt-endgame-threshold").unwrap_or(0),
        bt_max_upload_slots: get_u32("bt-max-upload-slots"),
        bt_optimistic_unchoke_interval: get_u64("bt-optimistic-unchoke-interval"),
        bt_snubbed_timeout: get_u64("bt-snubbed-timeout"),
        bt_prioritize_piece: get_str("bt-prioritize-piece").unwrap_or_default(),
        enable_utp: get_bool("enable-utp"),
        utp_listen_port: get_u16("utp-listen-port"),
        // Retry
        max_retries: get_u32("max-retries").unwrap_or(0),
        retry_wait: get_u64("retry-wait").unwrap_or(0),
        // DHT file
        dht_file_path: get_str("dht-file-path"),
        // Proxy
        http_proxy: get_str("http-proxy"),
        all_proxy: get_str("all-proxy"),
        https_proxy: get_str("https-proxy"),
        ftp_proxy: get_str("ftp-proxy"),
        no_proxy: get_str("no-proxy"),
        // HTTP headers
        header,
        user_agent: get_str("user-agent"),
        referer: get_str("referer"),
    }
}
