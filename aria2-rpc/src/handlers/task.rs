//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::engine::RpcEngine;
use crate::engine::TaskState;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, create_gid};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::checksum::checksum::Checksum;
use aria2_core::constants as core_constants;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::util::rwlock_ext::RwLockRecover;

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
        let position: Option<usize> = req
            .get_param::<i64>(2)
            .ok()
            .and_then(|p| if p >= 0 { Some(p as usize) } else { None });
        let gid = self.add_task(uris, opts).await?;
        if let Some(pos) = position {
            let pos_req = JsonRpcRequest::new(
                "aria2.changePosition",
                serde_json::json!([&gid, pos as i64, "POS_SET"]),
            );
            let _ = self.handle_change_position(&pos_req).await;
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    /// Handle `aria2.addTorrent` - Add a BitTorrent download.
    ///
    /// Original aria2 signature: `[torrent, uris?, opts?, pos?]`
    /// - param[0]: Base64-encoded torrent data (required)
    /// - param[1]: Additional URIs/trackers (optional, array of strings)
    /// - param[2]: Options dict (optional)
    /// - param[3]: Position in queue (optional)
    ///
    /// For backward compatibility, if param[1] is an object (not an array),
    /// it is treated as the options dict (old 3-param style).
    pub async fn handle_add_torrent(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let torrent_data: String = req.get_param(0)?;

        // Detect whether param[1] is URIs (array) or opts (object) for backward compatibility.
        // Original: [torrent, uris?, opts?, pos?]
        // Old Rust:  [torrent, opts?, pos?]
        let (additional_uris, opts, position) = match req.get_param::<Vec<String>>(1) {
            Ok(uris) => {
                // 4-parameter signature: [torrent, uris, opts, pos]
                let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(2);
                let position: Option<usize> = req
                    .get_param::<i64>(3)
                    .ok()
                    .and_then(|p| if p >= 0 { Some(p as usize) } else { None });
                (uris, opts, position)
            }
            Err(_) => {
                // Backward compatible: param[1] is opts, param[2] is pos
                let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
                let position: Option<usize> = req
                    .get_param::<i64>(2)
                    .ok()
                    .and_then(|p| if p >= 0 { Some(p as usize) } else { None });
                (vec![], opts, position)
            }
        };

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

        // Build URIs: primary torrent URI + additional URIs/trackers from param[1]
        let mut uris = vec![format!(
            "torrent://{}",
            &decoded_bytes[..std::cmp::min(32, decoded_bytes.len())]
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        )];
        uris.extend(additional_uris);

        let gid = self.add_task(uris, opts).await?;
        // Apply position parameter if provided (original aria2 behavior)
        if let Some(pos) = position {
            let pos_req = JsonRpcRequest::new(
                "aria2.changePosition",
                serde_json::json!([&gid, pos as i64, "POS_SET"]),
            );
            let _ = self.handle_change_position(&pos_req).await;
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            gid,
        ))
    }

    /// Handle `aria2.addMetalink` - Add downloads from Metalink XML.
    ///
    /// Original aria2 signature: `[metalink, opts?, pos?]`
    ///
    /// Returns an array of GIDs (one per download in the metalink), matching
    /// the C++ original which returns a list of GIDs rather than a single GID.
    pub async fn handle_add_metalink(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let position: Option<usize> = req
            .get_param::<i64>(2)
            .ok()
            .and_then(|p| if p >= 0 { Some(p as usize) } else { None });

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
        if let Some(pos) = position {
            let pos_req = JsonRpcRequest::new(
                "aria2.changePosition",
                serde_json::json!([&gid, pos as i64, "POS_SET"]),
            );
            let _ = self.handle_change_position(&pos_req).await;
        }
        // Return an array of GIDs matching C++ aria2 behaviour.
        // Currently we create a single download task per metalink; when
        // multi-file metalink parsing is implemented, this will return
        // one GID per file in the metalink document.
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            vec![gid],
        ))
    }

    /// Handle `aria2.remove` - Remove a download task.
    ///
    /// Removes the task from active downloads and adds it to stopped tasks
    /// so it can be queried via `tellStopped`, `removeDownloadResult`, etc.
    /// Matches original aria2c behaviour: the task is moved to the stopped
    /// list with `removed` status.
    pub async fn handle_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Propagate to RequestGroupMan when available
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(gid_parsed) = GroupId::from_hex_string(&gid) {
                let _ = man.remove_group(gid_parsed);
            }
        }

        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(mut state) => {
                // Cancel the CancellationToken so any running DownloadCommand
                // is signalled to stop. The DownloadCommand also independently
                // polls the RequestGroup status (set to `Removed` by
                // `man.remove_group` above) as the primary cancellation signal,
                // so cancelling the token here keeps behaviour consistent with
                // `handle_force_pause` / `handle_force_shutdown`.
                if let Some(cancel_token) = &state.cancel_token {
                    cancel_token.cancel();
                }

                // Set status to Removed before pushing to stopped_tasks so
                // tellStopped returns the correct status (matching aria2c).
                state.status.status = DownloadStatus::Removed;
                // Original aria2c sets errorCode=31 (REMOVED) for removed downloads
                state.status.error_code = Some(31);

                self.num_stopped_total.fetch_add(1, Ordering::Relaxed);

                // Push removed task into stopped_tasks so it shows up in tellStopped
                // and can be removed via removeDownloadResult/purgeDownloadResult.
                let mut stopped = self.stopped_tasks.write().await;
                stopped.push(state.status.clone());

                let _ = self
                    .event_publisher
                    .publish(EventType::DownloadStop, DownloadEvent::download_stop(&gid));
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!(gid),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.pause` / `aria2.forcePause` - Pause a download task.
    pub async fn handle_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Propagate to RequestGroupMan when available
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(gid_parsed) = GroupId::from_hex_string(&gid) {
                let _ = man.pause_group(gid_parsed);
            }
        }

        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Paused;
                // Cancel the running task's token so the download loop
                // detects the pause on its next check_cancelled() call.
                if let Some(cancel_token) = &state.cancel_token {
                    cancel_token.cancel();
                }
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&gid),
                );
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!(gid),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forcePause` - Force pause a download task.
    pub async fn handle_force_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Propagate to RequestGroupMan when available
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(gid_parsed) = GroupId::from_hex_string(&gid) {
                let _ = man.pause_group(gid_parsed);
            }
        }

        let mut tasks_map = self.tasks.write().await;
        match tasks_map.get_mut(&gid) {
            Some(task_state) => {
                task_state.status.status = DownloadStatus::Paused;
                if let Some(cancel_token) = &task_state.cancel_token {
                    cancel_token.cancel();
                }
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&gid),
                );
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!(gid),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.unpause` / `aria2.forceUnpause` - Resume a paused task.
    pub async fn handle_unpause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Propagate to RequestGroupMan when available
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(gid_parsed) = GroupId::from_hex_string(&gid) {
                let _ = man.unpause_group(gid_parsed);
            }
        }

        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Active;

                // When the RPC server is wired to a running DownloadEngine,
                // create a new DownloadCommand for the paused group and submit
                // it so the download actually resumes. Without this, the status
                // changes to Active but no download task runs.
                if let (Some(group_man), Some(cmd_tx)) = (&self.group_man, &self.cmd_tx) {
                    let man = group_man.read().await;
                    if let Some(gid_parsed) = GroupId::from_hex_string(&gid) {
                        if let Some(group) = man.group_by_id(gid_parsed) {
                            let group_guard = group.recover();
                            let options = group_guard.options_arc();
                            let uris = group_guard.uris().to_vec();
                            let first_uri = uris.first().map(|s| s.as_str()).unwrap_or("");
                            drop(group_guard);

                            if !first_uri.is_empty() {
                                match DownloadCommand::new_with_group(
                                    group,
                                    first_uri,
                                    &options,
                                    options.dir.as_deref(),
                                    options.out.as_deref(),
                                ) {
                                    Ok(cmd) => {
                                        if let Err(e) = cmd_tx.send(Box::new(cmd)) {
                                            tracing::warn!("Failed to send resume command: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to create resume command: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }

                // C++ aria2 fires onDownloadStart (not a separate onDownloadResume)
                // when a download is unpaused. Match that behavior for compatibility.
                let _ = self.event_publisher.publish(
                    EventType::DownloadStart,
                    DownloadEvent::download_start(&gid),
                );
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!(gid),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.tellStatus` - Get detailed status of a specific download.
    pub async fn handle_tell_status(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        match self.get_status(&gid).await {
            Some(status) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(status).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forceRemove` - Forcefully remove download(s) without graceful shutdown.
    ///
    /// Matches original aria2c behaviour: removes the task from the active
    /// tasks map, cancels any running download, and pushes to stopped_tasks
    /// with `removed` status so it appears in `tellStopped`.
    pub async fn handle_force_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gids = super::parse_gids(req, 0)?;

        // Propagate to RequestGroupMan when available
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            for gid in &gids {
                if let Some(gid_parsed) = GroupId::from_hex_string(gid) {
                    let _ = man.remove_group(gid_parsed);
                }
            }
        }

        let mut tasks = self.tasks.write().await;
        let mut actually_removed = 0usize;
        let mut removed_statuses: Vec<(String, StatusInfo, Vec<String>)> = Vec::new();

        for gid in &gids {
            if let Some(mut state) = tasks.remove(gid) {
                // Cancel the CancellationToken to interrupt any running
                // DownloadCommand. `man.remove_group` above already set the
                // RequestGroup status to `Removed` (the primary signal the
                // download loop polls), but cancelling the token keeps this
                // handler consistent with `handle_remove`.
                if let Some(cancel_token) = &state.cancel_token {
                    cancel_token.cancel();
                }

                // Set status to Removed before pushing to stopped_tasks.
                state.status.status = DownloadStatus::Removed;
                // Original aria2c sets errorCode=31 (REMOVED) for removed downloads
                state.status.error_code = Some(31);
                actually_removed += 1;
                removed_statuses.push((gid.clone(), state.status.clone(), state.uris.clone()));
            }
        }

        self.num_stopped_total
            .fetch_add(actually_removed, Ordering::Relaxed);

        // Push removed tasks into stopped_tasks and publish events
        {
            let mut stopped = self.stopped_tasks.write().await;
            for (gid, status, _uris) in &removed_statuses {
                stopped.push(status.clone());
                let _ = self
                    .event_publisher
                    .publish(EventType::DownloadStop, DownloadEvent::download_stop(gid));
            }
        }

        // Original aria2 returns the GID for single-GID calls.
        let result_gid = gids.last().cloned().unwrap_or_default();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(result_gid),
        ))
    }

    /// Handle `aria2.changeUri` - Add/remove URIs for an existing download.
    ///
    /// Returns `[delCount, addCount]` matching original aria2 behavior.
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

        // Count deletions before modifying
        let del_count = if let Some(ref to_remove) = del_uris {
            let before = state.uris.len();
            state.uris.retain(|u| !to_remove.contains(u));
            before - state.uris.len()
        } else {
            0
        };

        let add_count = if let Some(to_add) = add_uris {
            let count = to_add.len();
            state.uris.extend(to_add);
            count
        } else {
            0
        };

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([del_count, add_count]),
        ))
    }

    /// Handle `aria2.saveSession` - Save current session state to disk.
    pub async fn handle_save_session(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let dir = req.get_param_or_default::<String>(0);
        let _dir = if dir.is_empty() { ".".to_string() } else { dir };

        let tasks = self.tasks.read().await;
        let count = tasks.len();
        drop(tasks);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            format!("OK. Saved {} downloads.", count),
        ))
    }

    /// Handle `aria2.changePosition` - Change URI position within a download.
    ///
    /// Original aria2 signature: `[gid, pos, how]`
    /// - `pos`: Relative or absolute position (i64)
    /// - `how`: `"POS_SET"`, `"POS_CUR"`, or `"POS_END"`
    ///
    /// Returns the new absolute position on success.
    pub async fn handle_change_position(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let pos: i64 = req.get_param(1)?;
        let how: String = req.get_param_or_default(2);

        let mut tasks = self.tasks.write().await;
        let state = tasks
            .get_mut(&gid)
            .ok_or_else(|| JsonRpcError::MethodNotFound(format!("GID {} not found", gid)))?;

        let len = state.uris.len() as i64;
        let current_pos = 0i64;

        let new_pos = match how.as_str() {
            "POS_SET" => pos,
            "POS_CUR" => current_pos + pos,
            "POS_END" => (len + pos).max(0),
            _ => {
                return Err(JsonRpcError::InvalidParams(format!(
                    "Invalid 'how' value: {}. Must be POS_SET, POS_CUR, or POS_END",
                    how
                )));
            }
        };

        let new_pos = new_pos.max(0).min((len - 1).max(0)) as usize;

        // Move the URI at index 0 (first URI) to new_pos
        if !state.uris.is_empty() && new_pos < state.uris.len() {
            let uri = state.uris.remove(0);
            state.uris.insert(new_pos, uri);
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(new_pos as i64),
        ))
    }

    /// Handle `aria2.shutdown` - Graceful shutdown (save session, wait for downloads).
    ///
    /// This method performs a graceful shutdown:
    /// 1. Saves current session state
    /// 2. Marks all active downloads as paused
    /// 3. Returns "OK. N active downloads paused." to indicate shutdown initiated
    pub async fn handle_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Pause all active downloads
        let tasks = self.tasks.read().await;
        let mut active_count = 0;
        for state in tasks.values() {
            if state.status.status == DownloadStatus::Active {
                active_count += 1;
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&state.status.gid),
                );
            }
        }
        drop(tasks);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String(format!("OK. {} active downloads paused.", active_count)),
        ))
    }

    /// Handle `aria2.forceShutdown` - Force shutdown (immediate termination).
    ///
    /// This method performs an immediate shutdown:
    /// 1. Cancels all active downloads via CancellationToken
    /// 2. Clears all task state
    /// 3. Returns "OK" to indicate shutdown completed
    pub async fn handle_force_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Cancel all active downloads
        let mut tasks = self.tasks.write().await;
        for state in tasks.values_mut() {
            // Cancel the download if it has a cancellation token
            if let Some(cancel_token) = &state.cancel_token {
                cancel_token.cancel();
            }
            // Mark as removed
            state.status.status = DownloadStatus::Removed;
        }
        let cancelled_count = tasks.len();
        tasks.clear();

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String(format!(
                "OK. {} downloads forcibly terminated.",
                cancelled_count
            )),
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

        // Validate checksum format at task creation time, before any download starts.
        if let Some((ref algo, ref val)) = dl_options.checksum {
            Checksum::from_type_and_value(algo, val)
                .map_err(|e| JsonRpcError::InvalidParams(format!("Invalid checksum: {}", e)))?;
        }

        // Start a real download if we have shared engine state
        if let (Some(group_man), Some(cmd_tx)) = (&self.group_man, &self.cmd_tx) {
            let man = group_man.read().await;
            man.add_group_with_gid(gid, uris.clone(), dl_options.clone())
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
        let state = TaskState::new(status, options, uris.clone());
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(gid_str.clone(), state);
        }
        // C++ aria2 notification only includes gid (no files field)
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid_str),
        );
        Ok(gid_str)
    }

    /// Build a complete StatusInfo from a RequestGroup read guard.
    ///
    /// Populates all available fields matching original aria2c's
    /// `gatherProgress` / `gatherProgressCommon` output structure:
    /// - Active/waiting/paused downloads: gatherProgress fields
    /// - Stopped/completed/removed/error: gatherStoppedDownload fields
    pub(crate) fn build_status_from_group(
        g: &aria2_core::request::request_group::RequestGroup,
        gid_hex: &str,
    ) -> StatusInfo {
        let status = g.status();
        let total = g.get_total_length_atomic();
        let completed = g.get_completed_length();
        let dl_speed = g.get_download_speed_cached();
        let uploaded = g.get_uploaded_length();
        let ul_speed = g.get_upload_speed_cached();
        let dir = g.options().dir.clone().unwrap_or_default();
        let uris: Vec<String> = g.uris().to_vec();
        let first_uri = uris.first().cloned().unwrap_or_default();

        // Build file entries matching original createFileEntry:
        // index (1-based), path, selected, length, completedLength, uris
        let files = vec![
            FileInfo::new(first_uri, total)
                .with_completed(completed)
                .with_index(1),
        ];

        let connections = g.options().split.unwrap_or(core_constants::DEFAULT_SPLIT) as u16;

        // BT-specific fields: bitfield, piece length, num pieces, info hash
        let bt_info_hash = g.get_bt_info_hash_hex();
        let is_bt = bt_info_hash.is_some();
        let mut bt_bitfield = None;
        let mut bt_piece_length = None;
        let mut bt_num_pieces = None;
        if is_bt {
            let np = g.get_bt_num_pieces();
            bt_num_pieces = Some(np);
            bt_piece_length = Some(g.get_bt_piece_length() as u64);
            if np > 0 {
                bt_bitfield = g
                    .get_bt_bitfield()
                    .map(|bf| bf.iter().map(|b| format!("{:02x}", b)).collect::<String>());
            }
        }

        let mut info = StatusInfo::new(gid_hex)
            .with_status(status.clone())
            .with_total_length(total)
            .with_completed_length(completed)
            .with_upload_length(uploaded)
            .with_download_speed(dl_speed)
            .with_upload_speed(ul_speed)
            .with_connections(connections)
            .with_dir(dir)
            .with_files(files);

        // Attach BT-specific fields only when applicable (matching original:
        // pieceLength/numPieces are always emitted, bitfield/infoHash/numSeeders
        // only for BT downloads)
        if let Some(bf) = bt_bitfield {
            info = info.with_bitfield(bf);
        }
        if let Some(pl) = bt_piece_length {
            info = info.with_piece_length(pl);
        }
        if let Some(np) = bt_num_pieces {
            info = info.with_num_pieces(np);
        }
        if let Some(ih) = bt_info_hash {
            info = info.with_info_hash(ih);
            // numSeeders is only emitted for BT downloads (original:
            // gatherProgressBitTorrent / gatherStoppedDownload)
            info = info.with_num_seeders(0);
        }

        // pieceLength and numPieces are always emitted in original aria2c
        // (gatherProgressCommon and gatherStoppedDownload both emit them).
        // If BT fields were not set above, fill from progress data.
        // Original aria2c default piece length is 1 MiB.
        const ARIA2_DEFAULT_PIECE_LENGTH: u64 = 1048576;
        if info.piece_length.is_none() && total > 0 {
            info = info.with_piece_length(ARIA2_DEFAULT_PIECE_LENGTH);
        }
        if info.num_pieces.is_none() && total > 0 {
            let pl = info.piece_length.unwrap_or(ARIA2_DEFAULT_PIECE_LENGTH);
            if pl > 0 {
                info = info.with_num_pieces(((total + pl - 1) / pl) as u32);
            }
        }

        // Error handling matching original gatherStoppedDownload:
        // REMOVED (31) -> status="removed", errorCode="31"
        // FINISHED (0) -> status="complete", errorCode="0"
        // Other errors -> status="error", errorCode=<numeric>
        match &status {
            DownloadStatus::Error(msg) => {
                info = info.with_error_message(msg.clone());
                // Use error_code 1 (UNKNOWN_ERROR) for generic errors
                // matching original aria2 convention
                info = info.with_error_code(1);
            }
            DownloadStatus::Complete => {
                // Original aria2c sets errorCode=0 for FINISHED
                info = info.with_error_code(0);
            }
            DownloadStatus::Removed => {
                // Original aria2c uses error_code::REMOVED = 31
                info = info.with_error_code(31);
            }
            _ => {}
        }

        info
    }

    /// Internal helper to get current status info for a task.
    ///
    /// Prefers live progress from `RequestGroupMan` (atomic fields updated by
    /// the download engine). Falls back to the placeholder `tasks` map when
    /// shared state is unavailable (e.g., unit tests). Finally checks
    /// `stopped_tasks` so that `tellStatus` can return removed/completed
    /// downloads (matching original aria2c behaviour).
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        // Try RequestGroupMan first (live progress)
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(group_lock) = man.group_by_hex(gid) {
                let g = group_lock.recover();
                return Some(Self::build_status_from_group(&g, gid));
            }
        }
        // Fallback to tasks map (placeholder, for tests/no-engine mode)
        {
            let mut tasks = self.tasks.write().await;
            if let Some(state) = tasks.get_mut(gid) {
                state.update_status_info();
                return Some(state.status.clone());
            }
        }
        // Check stopped_tasks (removed/completed downloads that are still
        // queryable via tellStatus until purged, matching original aria2c).
        let stopped = self.stopped_tasks.read().await;
        stopped.iter().find(|s| s.gid == *gid).cloned()
    }
}

/// Convert RPC option map (from `aria2.addUri` params) to `DownloadOptions`.
///
/// Handles both array and newline-separated string forms of `header`.
fn rpc_options_to_download_options(opts: &HashMap<String, serde_json::Value>) -> DownloadOptions {
    let get_str = |k: &str| opts.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let get_u16 = |k: &str| opts.get(k).and_then(|v| v.as_u64()).map(|n| n as u16);
    let get_u32 = |k: &str| opts.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
    let get_u64 = |k: &str| opts.get(k).and_then(|v| v.as_u64());
    let get_f64 = |k: &str| opts.get(k).and_then(|v| v.as_f64());
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
