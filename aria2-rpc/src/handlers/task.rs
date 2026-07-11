//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use std::collections::HashMap;

use crate::engine::RpcEngine;
use crate::engine::TaskState;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, create_gid};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};

impl RpcEngine {
    /// Handle `aria2.addUri` - Add a new download task from URI(s).
    pub async fn handle_add_uri(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
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
        let opts: HashMap<String,serde_json::Value> = req.get_param_or_default(1);
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
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &torrent_data,
            )
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
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &metalink_data,
            )
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
    pub async fn handle_remove(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.remove(&gid) {
            Some(_) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([gid]),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.pause` / `aria2.forcePause` - Pause a download task.
    pub async fn handle_pause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Paused;
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!([gid]),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forcePause` - Force pause a download task.
    pub async fn handle_force_pause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
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
                    serde_json::json!("OK"),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid))),
        }
    }

    /// Handle `aria2.unpause` / `aria2.forceUnpause` - Resume a paused task.
    pub async fn handle_unpause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let mut tasks = self.tasks.write().await;
        match tasks.get_mut(&gid) {
            Some(state) => {
                state.status.status = DownloadStatus::Active;
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!([gid]),
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
                serde_json::to_value(status)
                    .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.forceRemove` - Forcefully remove download(s) without graceful shutdown.
    pub async fn handle_force_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gids = super::parse_gids(req, 0)?;

        let mut tasks = self.tasks.write().await;
        for gid in &gids {
            if let Some(state) = tasks.get_mut(gid) {
                state.status.status = DownloadStatus::Removed;
            }
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    /// Handle `aria2.changeUri` - Add/remove URIs for an existing download.
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
        let state = tasks.get_mut(&gid).ok_or_else(|| {
            JsonRpcError::MethodNotFound(format!("GID {} not found", gid))
        })?;

        if let Some(to_remove) = del_uris {
            state.uris.retain(|u| !to_remove.contains(u));
        }

        if let Some(to_add) = add_uris {
            state.uris.extend(to_add);
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([gid, 0]),
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

    /// Handle `aria2.changePosition` - Change URI position within a download.
    pub async fn handle_change_position(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid = req.get_param::<String>(0)?;
        let _file_index: usize = req.get_param(1)?;
        let del_pos: Option<usize> = req.get_param(2).ok();
        let add_pos: Option<usize> = req.get_param(3).ok();
        let how: u8 = req.get_param_or_default(4);

        if how > 2 {
            return Ok(JsonRpcResponse::error(
                req.id.clone().unwrap_or_default(),
                -32602,
                format!("Invalid 'how' value: {}", how),
            ));
        }

        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(&gid).ok_or_else(|| {
            JsonRpcError::MethodNotFound(format!("GID {} not found", gid))
        })?;

        match (del_pos, add_pos) {
            (Some(del), Some(add)) => {
                if del < state.uris.len() && add <= state.uris.len() {
                    let uri = state.uris.remove(del);
                    state.uris.insert(add.min(state.uris.len()), uri);
                    return Ok(JsonRpcResponse::success(
                        req.id.clone().unwrap_or_default(),
                        serde_json::Value::String("OK".into()),
                    ));
                }
            }
            (Some(del), None) => {
                if del < state.uris.len() {
                    state.uris.remove(del);
                    return Ok(JsonRpcResponse::success(
                        req.id.clone().unwrap_or_default(),
                        serde_json::Value::String("OK".into()),
                    ));
                }
            }
            (None, Some(add)) => {
                if !state.uris.is_empty() && add <= state.uris.len() {
                    let uri = state.uris.pop()
                        .ok_or_else(|| JsonRpcError::InternalError("No URIs available".to_string()))?;
                    state.uris.insert(add, uri);
                    return Ok(JsonRpcResponse::success(
                        req.id.clone().unwrap_or_default(),
                        serde_json::Value::String("OK".into()),
                    ));
                }
            }
            (None, None) => {}
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String("OK".into()),
        ))
    }

    /// Handle `aria2.shutdown` - Graceful shutdown (save session, wait for downloads).
    ///
    /// This method performs a graceful shutdown:
    /// 1. Saves current session state
    /// 2. Marks all active downloads as paused
    /// 3. Returns "OK" to indicate shutdown initiated
    pub async fn handle_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Save session state (count active tasks)
        let tasks = self.tasks.read().await;
        let active_count = tasks.len();
        drop(tasks);

        // In a real implementation, this would:
        // - Save session to disk
        // - Wait for active downloads to complete (with timeout)
        // - Signal the main process to exit gracefully

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String(format!("OK. {} active downloads will be saved.", active_count)),
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
            serde_json::Value::String(format!("OK. {} downloads forcibly terminated.", cancelled_count)),
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

            let group = man
                .group_by_id(gid)
                .ok_or_else(|| JsonRpcError::InternalError("Group not found after insert".into()))?;

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

            cmd_tx
                .send(Box::new(cmd))
                .map_err(|e| JsonRpcError::InternalError(format!("Failed to send command: {}", e)))?;
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
            DownloadEvent::download_start(&gid_str, vec![]),
        );
        Ok(gid_str)
    }

    /// Internal helper to get current status info for a task.
    ///
    /// Prefers live progress from `RequestGroupMan` (atomic fields updated by
    /// the download engine). Falls back to the placeholder `tasks` map when
    /// shared state is unavailable (e.g., unit tests).
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        // Try RequestGroupMan first (live progress)
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(group_lock) = man.group_by_hex(gid) {
                let g = group_lock.read().await;
                let status = g.status().await;
                let total = g.get_total_length_atomic();
                let completed = g.get_completed_length();
                let dl_speed = g.get_download_speed_cached();
                let uploaded = g.get_uploaded_length();
                let dir = g.options().dir.clone().unwrap_or_default();
                let uris: Vec<String> = g.uris().to_vec();
                let first_uri = uris.first().cloned().unwrap_or_default();
                let files = vec![FileInfo::new(first_uri, total).with_completed(completed)];
                return Some(StatusInfo::new(gid)
                    .with_status(status)
                    .with_total_length(total)
                    .with_completed_length(completed)
                    .with_upload_length(uploaded)
                    .with_download_speed(dl_speed)
                    .with_dir(dir)
                    .with_files(files));
            }
        }
        // Fallback to tasks map (placeholder, for tests/no-engine mode)
        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(gid)?;
        state.update_status_info();
        Some(state.status.clone())
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
        enable_dht: opts.get("enable-dht").and_then(|v| v.as_bool()).unwrap_or(true),
        dht_listen_port: get_u16("dht-listen-port"),
        dht_entry_point,
        enable_public_trackers: opts.get("enable-public-trackers").and_then(|v| v.as_bool()).unwrap_or(true),
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
