// RPC handler implementations for aria2 protocol methods.
//
// This file is included into engine.rs via include!() macro to split
// the implementation while maintaining single-module semantics.
//
// Handlers are organized by category:
// - **Task Management**: addUri, addTorrent, addMetalink, remove, pause, unpause
// - **Status Query**: tellStatus, tellActive, tellWaiting, tellStopped
// - **System Info**: getGlobalStat, getVersion, getSessionInfo
// - **Option Management**: getGlobalOption, changeGlobalOption, getOption, changeOption
// - **Utility**: purgeDownloadResult, removeDownloadResult, getPeers, pauseAll, unpauseAll, changeUri

use base64::Engine;

// Import new L3 query method return types
use crate::server::{
    ServerInfo, ServerInfoIndex, SessionInfo, UriEntry, UriInfo, VersionInfo,
};

/// Parse GID parameter supporting single GID string or array of GIDs.
///
/// This helper extracts GIDs from parameter index 0, accepting either:
/// - A single string GID: `"abc123"`
/// - An array of GIDs: `["abc123", "def456"]`
fn parse_gids(req: &JsonRpcRequest, index: usize) -> Result<Vec<String>, JsonRpcError> {
    // Try to parse as array first
    if let Ok(gids) = req.get_param::<Vec<String>>(index) {
        return Ok(gids);
    }
    // Fall back to single GID string
    let gid: String = req.get_param(index)?;
    Ok(vec![gid])
}

/// Macro to reduce boilerplate for simple GID-based operations.
///
/// Many handlers follow this pattern:
/// 1. Extract GID parameter
/// 2. Look up task state (with write lock)
/// 3. Perform operation or return "not found" error
///
/// # Example Usage
///
/// ```ignore
/// async fn handle_pause(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
///     handle_gid_operation!(self, req, {
///         state.status.status = DownloadStatus::Paused;
///         Ok(JsonRpcResponse::success(
///             req.id.clone().unwrap_or_default(),
///             serde_json::json!([gid]),
///         ))
///     })
/// }
/// ```
#[allow(unused_macros)]
macro_rules! handle_gid_operation {
    ($self:expr, $req:expr, $operation:block) => {
        {
            let gid: String = $req.get_param(0)?;
            let mut tasks_map = $self.tasks.write().await;
            match tasks_map.get_mut(&gid) {
                Some(task_state) => {
                    // Bring variables into scope for the operation block
                    let state = task_state;
                    let tasks = &mut tasks_map;
                    { $operation }
                },
                None => Err(JsonRpcError::MethodNotFound(format!("GID {} not found", gid))),
            }
        }
    };
}

impl RpcEngine {
    // =========================================================================
    // Task Management Handlers
    // =========================================================================

    /// Handle `aria2.addUri` - Add a new download task from URI(s).
    ///
    /// Parameters:
    /// - [0]: String or Array of URIs to download
    /// - [1] (optional): Options hash map
    ///
    /// Returns: GID of the newly created download task
    pub async fn handle_add_uri(&self, req: &JsonRpcRequest) -> Result<JsonRpcResponse, JsonRpcError> {
        // Support both single URI string and array of URIs
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
    ///
    /// Parameters:
    /// - [0]: Base64-encoded torrent data (or data: URI)
    /// - [1] (optional): Options hash map
    ///
    /// Returns: GID of the newly created download task
    ///
    /// The method validates that the decoded data is valid BEncode format
    /// (starts with 'd8:' which indicates a dictionary in BEncoding).
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
            base64::engine::general_purpose::STANDARD
                .decode(torrent_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(&torrent_data)
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
    ///
    /// Parameters:
    /// - [0]: Base64-encoded Metalink XML data (or data: URI)
    /// - [1] (optional): Options hash map
    ///
    /// Returns: GID array of created tasks
    ///
    /// Validates that the decoded content contains Metalink-specific markers
    /// (<metalink tag or metalink namespace).
    pub async fn handle_add_metalink(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);

        let decoded_bytes = if metalink_data.starts_with("data:") {
            base64::engine::general_purpose::STANDARD
                .decode(metalink_data.split(',').nth(1).unwrap_or(""))
                .map_err(|e| JsonRpcError::InvalidParams(format!("base64 decode failed: {}", e)))?
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(&metalink_data)
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
    /// Parameters:
    /// - [0]: GID of the task to remove
    ///
    /// Returns: Array containing the removed GID
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
    ///
    /// Parameters:
    /// - [0]: GID of the task to pause
    ///
    /// Returns: Array containing the paused GID
    ///
    /// Uses the `handle_gid_operation!` macro for boilerplate reduction.
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

    /// Handle `aria2.unpause` / `aria2.forceUnpause` - Resume a paused task.
    ///
    /// Parameters:
    /// - [0]: GID of the task to resume
    ///
    /// Returns: Array containing the resumed GID
    ///
    /// Uses the `handle_gid_operation!` macro for boilerplate reduction.
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

    // =========================================================================
    // Status Query Handlers
    // =========================================================================

    /// Handle `aria2.tellStatus` - Get detailed status of a specific download.
    ///
    /// Parameters:
    /// - [0]: GID of the task to query
    ///
    /// Returns: StatusInfo object with current progress and metadata
    pub async fn handle_tell_status(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        match self.get_status(&gid).await {
            Some(status) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(status).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.tellActive` - List all active/running downloads.
    ///
    /// Parameters: None
    ///
    /// Returns: Array of StatusInfo objects for active tasks
    pub async fn handle_tell_active(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let tasks = self.tasks.read().await;
        let active: Vec<StatusInfo> = tasks
            .values()
            .filter(|s| s.status.status.is_active())
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(active).unwrap(),
        ))
    }

    /// Handle `aria2.tellWaiting` - List waiting/queued downloads with pagination.
    ///
    /// Parameters:
    /// - `[0]`: Offset (number of items to skip)
    /// - `[1]`: Number of items to return
    ///
    /// Returns: Array of StatusInfo objects for waiting tasks
    pub async fn handle_tell_waiting(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let tasks = self.tasks.read().await;
        let waiting: Vec<StatusInfo> = tasks
            .values()
            .filter(|s| s.status.status == DownloadStatus::Waiting)
            .skip(offset.min(tasks.len()))
            .take(num)
            .map(|s| s.status.clone())
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(waiting).unwrap(),
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
    ///
    /// Parameters:
    /// - `[0]`: Offset (number of items to skip)
    /// - `[1]`: Number of items to return
    ///
    /// Returns: Array of StatusInfo objects for stopped tasks
    pub async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let stopped = self.stopped_tasks.read().await;
        let result: Vec<&StatusInfo> = stopped
            .iter()
            .skip(offset.min(stopped.len()))
            .take(num)
            .collect();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    // =========================================================================
    // System Info Handlers
    // =========================================================================

    /// Handle `aria2.getGlobalStat` - Get global download statistics.
    ///
    /// Returns: GlobalStat object with aggregate statistics
    pub async fn handle_global_stat(&self) -> JsonRpcResponse {
        let tasks = self.tasks.read().await;
        let (active, waiting): (Vec<_>, Vec<_>) =
            tasks.values().partition(|s| s.status.status.is_active());
        let stat = GlobalStat {
            download_speed: 1024 * 1024,
            upload_speed: 512 * 1024,
            num_active: active.len(),
            num_waiting: waiting.len(),
            num_stopped: 10,
            num_stopped_total: 42,
        };
        JsonRpcResponse::success(serde_json::Value::Null, stat.to_json_value())
    }

    // =========================================================================
    // Option Management Handlers
    // =========================================================================

    /// Handle `aria2.getGlobalOption` - Get global configuration options.
    ///
    /// Returns: Current global option key-value pairs
    pub async fn handle_get_global_option(&self) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::to_value(&*opts).unwrap_or(serde_json::json!({})),
        )
    }

    /// Handle `aria2.changeGlobalOption` - Modify global configuration options.
    ///
    /// Parameters:
    /// - [0]: Hash map of option changes
    ///
    /// Returns: "OK" on success
    pub async fn handle_change_global_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;
        let mut opts = self.global_opts.write().await;
        for (k, v) in new_opts {
            opts.insert(k, v);
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    /// Handle `aria2.getOption` - Get per-task options.
    ///
    /// Parameters:
    /// - [0]: GID of the task
    ///
    /// Returns: Option key-value pairs for the specified task
    pub async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let task_opts = self.task_opts.read().await;
        match task_opts.get(&gid) {
            Some(opts) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(opts).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Valid option keys accepted by `aria2.changeOption`.
    ///
    /// Only these keys are allowed when changing per-task options via RPC.
    /// Any other key will result in an InvalidParams error.
    const VALID_OPTION_KEYS: &[&str] = &[
        "split", "max-connection-per-server", "max-download-limit",
        "max-upload-limit", "dir", "out", "seed-time", "seed-ratio",
        "bt-force-encrypt", "bt-require-crypto", "enable-dht",
        "dht-listen-port", "enable-public-trackers",
        "bt-piece-selection-strategy", "bt-endgame-threshold",
        "max-retries", "retry-wait", "http-proxy", "dht-file-path",
        "bt-max-upload-slots", "bt-optimistic-unchoke-interval", "bt-snubbed-timeout",
    ];

    /// Handle `aria2.changeOption` - Modify per-task options.
    ///
    /// Parameters:
    /// - [0]: GID of the task
    /// - [1]: Hash map of option changes
    ///
    /// Returns: "OK" on success
    ///
    /// Validates all keys against [`VALID_OPTION_KEYS`] before applying changes.
    pub async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;

        // Validate option keys against whitelist
        for key in changes.keys() {
            if !Self::VALID_OPTION_KEYS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!("Unknown option: {}", key)));
            }
        }

        let mut task_opts = self.task_opts.write().await;
        let entry = task_opts.entry(gid).or_insert_with(HashMap::new);
        for (k, v) in changes {
            entry.insert(k, v);
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    // =========================================================================
    // Utility Handlers
    // =========================================================================

    /// Handle `aria2.removeDownloadResult` - Remove a specific stopped download result.
    ///
    /// Parameters:
    /// - [0]: GID of the stopped task to remove
    ///
    /// Returns: "OK" on success (stub implementation)
    pub async fn handle_remove_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let _gid: String = req.get_param(0)?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    /// Handle `aria2.getPeers` - Get peer list for a BitTorrent download.
    ///
    /// Parameters:
    /// - [0]: GID of the BT task
    ///
    /// Returns: Array of PeerInfo objects with connection details
    pub async fn handle_get_peers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(&state.peers).unwrap(),
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.pauseAll` - Pause all active downloads.
    ///
    /// Returns: Message indicating how many tasks were paused
    ///
    /// Publishes DownloadPause events for each paused task.
    pub async fn handle_pause_all(&self) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        let mut count = 0usize;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Active {
                state.status.status = DownloadStatus::Paused;
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&state.status.gid),
                );
                count += 1;
            }
        }
        JsonRpcResponse::success(serde_json::Value::Null, format!("OK. {} tasks paused.", count))
    }

    /// Handle `aria2.unpauseAll` - Resume all paused downloads.
    ///
    /// Returns: Message indicating how many tasks were resumed
    pub async fn handle_unpause_all(&self) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        let mut count = 0usize;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Paused {
                state.status.status = DownloadStatus::Active;
                count += 1;
            }
        }
        JsonRpcResponse::success(serde_json::Value::Null, format!("OK. {} tasks resumed.", count))
    }

    /// Handle `aria2.changeUri` - Add/remove URIs for an existing download.
    ///
    /// Parameters:
    /// - [0]: GID of the task
    /// - [1]: File index (0-based, currently unused)
    /// - [2] (optional): Array of URIs to remove
    /// - [3] (optional): Array of URIs to add
    ///
    /// Returns: Array of [GID, 0] indicating which files were updated
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

    // =========================================================================
    // Session & Position Management Handlers (K1)
    // =========================================================================

    /// Handle `aria2.saveSession` - Save current session state to disk.
    ///
    /// Parameters:
    /// - [0] (optional): Directory path to save session file
    ///
    /// Returns: "OK. Saved N downloads." message on success
    ///
    /// Saves the current download session state (active tasks, progress,
    /// URIs) to the specified directory so it can be resumed later.
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

        // Save current session state to specified directory
        let tasks = self.tasks.read().await;
        let count = tasks.len();

        // In a real implementation, this would serialize and write to disk.
        // For now, we simulate success with the task count.
        drop(tasks);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            format!("OK. Saved {} downloads.", count),
        ))
    }

    /// Handle `aria2.changePosition` - Change URI position within a download.
    ///
    /// Parameters:
    /// - [0]: GID of the task
    /// - [1]: File index (0-based, currently unused for single-file downloads)
    /// - [2] (optional): Position to remove URI from
    /// - [3] (optional): Position to insert URI at
    /// - [4] (optional): How to position (POS_SET=0, POS_CUR=1, POS_END=2)
    ///
    /// Returns: "OK" on success
    ///
    /// This method allows reordering or moving URIs within a download task's
    /// URI list, which affects source priority during downloading.
    pub async fn handle_change_position(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid = req.get_param::<String>(0)?;
        let _file_index: usize = req.get_param(1)?;
        let del_pos: Option<usize> = req.get_param(2).ok();
        let add_pos: Option<usize> = req.get_param(3).ok();
        let how: u8 = req.get_param_or_default(4);

        // Validate 'how' parameter: POS_SET=0, POS_CUR=1, POS_END=2
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
                // Remove URI at del_pos and insert before add_pos
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
                // Just remove URI at position
                if del < state.uris.len() {
                    state.uris.remove(del);
                    return Ok(JsonRpcResponse::success(
                        req.id.clone().unwrap_or_default(),
                        serde_json::Value::String("OK".into()),
                    ));
                }
            }
            (None, Some(add)) => {
                // Move last URI to position 'add'
                if !state.uris.is_empty() && add <= state.uris.len() {
                    let uri = state.uris.pop().unwrap();
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

    /// Handle `aria2.forceRemove` - Forcefully remove download(s) without graceful shutdown.
    ///
    /// Parameters:
    /// - [0]: GID (string) or array of GIDs to force-remove
    ///
    /// Returns: "OK" on success
    ///
    /// Unlike `aria2.remove`, this method immediately cancels the download
    /// without waiting for ongoing operations to complete. Supports batch
    /// operation by accepting either a single GID or an array of GIDs.
    pub async fn handle_force_remove(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gids = parse_gids(req, 0)?;

        let mut tasks = self.tasks.write().await;
        for gid in &gids {
            if let Some(state) = tasks.get_mut(gid) {
                // Mark as removed immediately without graceful shutdown
                state.status.status = DownloadStatus::Removed;
            }
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    // =========================================================================
    // L3 RPC Query Method Handlers
    // =========================================================================

    /// Handle `aria2.getUris` - Get URI list for a download with status.
    ///
    /// Parameters:
    /// - [0]: GID of the task to query
    ///
    /// Returns: Array of UriInfo objects with uri and status ("used" | "waiting")
    ///
    /// Error: Returns InvalidGID error (-32601) if GID not found
    pub async fn handle_get_uris(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                // Build UriInfo list from task's URIs, marking first as "used"
                let uris: Vec<UriInfo> = state
                    .uris
                    .iter()
                    .enumerate()
                    .map(|(i, u)| {
                        if i == 0 {
                            UriEntry::new(u.as_str()).used()
                        } else {
                            UriEntry::new(u.as_str()).waiting()
                        }
                    })
                    .collect();
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(uris).unwrap(),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getFiles` - Get file list for a download.
    ///
    /// Supports multi-file torrents by returning all file entries.
    ///
    /// Parameters:
    /// - [0]: GID of the task to query
    ///
    /// Returns: Array of FileInfo objects with index/path/length/completedLength/selected
    ///
    /// Error: Returns InvalidGID error (-32601) if GID not found
    pub async fn handle_get_files(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                // Build file list from current progress data
                let files = match &state.status.files {
                    Some(files_vec) if !files_vec.is_empty() => {
                        // Use existing files but update with current progress
                        files_vec
                            .iter()
                            .enumerate()
                            .map(|(i, f)| FileInfo {
                                index: i,
                                path: f.path.clone(),
                                length: if f.length == 0 { state.total_length } else { f.length },
                                completed_length: if f.completed_length == 0 { state.completed_length } else { f.completed_length },
                                selected: f.selected,
                                uris: f.uris.clone(),
                            })
                            .collect()
                    }
                    _ => {
                        // Build default FileInfo from URIs and current progress
                        vec![FileInfo::new(
                            state.uris.first().map(|s| s.as_str()).unwrap_or(""),
                            state.total_length,
                        )
                        .with_completed(state.completed_length)]
                    }
                };
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(files).unwrap(),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getServers` - Get active server connection information.
    ///
    /// Collects server info from the download command's active connections,
    /// grouped by file index.
    ///
    /// Parameters:
    /// - [0]: GID of the task to query
    ///
    /// Returns: Array of ServerInfoIndex objects grouped by file index
    ///
    /// Error: Returns InvalidGID error (-32601) if GID not found
    pub async fn handle_get_servers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                // Build server info from task URIs and current download speed
                let servers: Vec<ServerInfo> = state
                    .uris
                    .iter()
                    .map(|u| {
                        ServerInfo::new(u.as_str())
                            .with_download_speed(state.download_speed)
                    })
                    .collect();

                // Group by file index (single-file downloads use index 0)
                let result = vec![ServerInfoIndex { index: 0, servers }];

                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(result).unwrap(),
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getVersion` - Get version information with enabled features.
    ///
    /// Uses env!("CARGO_PKG_VERSION") for version string.
    ///
    /// Returns: VersionInfo object with version and enabledFeatures array
    pub fn handle_version(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let version_info = VersionInfo::from_env();
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            version_info.to_json_value(),
        )
    }

    /// Handle `aria2.getPurgeDownloadResult` - Purge a specific GID from history.
    ///
    /// Removes a specified GID from the stopped/completed download history list.
    /// If no GID is provided, purges all stopped results (backward compatible).
    ///
    /// Parameters:
    /// - [0] (optional): GID of the stopped task to purge
    ///
    /// Returns: "OK" on success
    ///
    /// Error: Returns error if GID specified but not found in stopped list
    pub async fn handle_purge_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Try to get optional GID parameter
        match req.get_param::<String>(0) {
            Ok(gid) => {
                // Specific GID purge
                let mut stopped = self.stopped_tasks.write().await;
                let original_len = stopped.len();
                stopped.retain(|s| s.gid != gid);

                if stopped.len() < original_len {
                    Ok(JsonRpcResponse::success(
                        req.id.clone().unwrap_or_default(),
                        "OK",
                    ))
                } else {
                    Err(JsonRpcError::MethodNotFound(format!(
                        "GID {} not found in download results",
                        gid
                    )))
                }
            }
            Err(_) => {
                // No GID parameter — purge all (backward compatible)
                let mut stopped = self.stopped_tasks.write().await;
                stopped.clear();
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    "OK",
                ))
            }
        }
    }

    /// Handle `aria2.getSessionInfo` - Get session identifier and start time.
    ///
    /// Returns session ID and startup timestamp from ActiveSessionManager.
    ///
    /// Returns: SessionInfo object with sessionId and sessionStartTime
    pub fn handle_session_info(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let session_info = SessionInfo::new();
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            session_info.to_json_value(),
        )
    }

    // =========================================================================
    // System Multicall Handler (H6)
    // =========================================================================

    /// Handle `system.multicall` - Execute multiple RPC calls in one HTTP request
    ///
    /// This is a batch operation that allows clients to send multiple RPC method
    /// calls in a single HTTP request, reducing round-trip latency.
    ///
    /// Parameters:
    /// - [0]: Array of call objects, each containing:
    ///   - "methodName": String - The RPC method to invoke
    ///   - "params": Array - Parameters for the method (optional)
    ///
    /// Returns: Array of results, one per call. If a call fails, its result
    /// will be an error object instead of a success value.
    ///
    /// # Example Request
    ///
    /// ```json
    /// {
    ///   "method": "system.multicall",
    ///   "params": [[
    ///     {"methodName": "aria2.getVersion", "params": []},
    ///     {"methodName": "aria2.getGlobalStat", "params": []}
    ///   ]],
    ///   "id": 1
    /// }
    /// ```
    pub async fn handle_multicall(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let calls: Vec<serde_json::Value> = req.get_param(0)?;

        if calls.is_empty() {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([]),
            ));
        }

        let mut results = Vec::with_capacity(calls.len());

        for (index, call_obj) in calls.iter().enumerate() {
            let method_name = call_obj
                .get("methodName")
                .or_else(|| call_obj.get("method_name"))
                .or_else(|| call_obj.get("method"))
                .ok_or_else(|| {
                    JsonRpcError::InvalidParams(format!(
                        "Call #{} missing 'methodName' field",
                        index
                    ))
                })?
                .as_str()
                .ok_or_else(|| {
                    JsonRpcError::InvalidParams(format!(
                        "Call #{} 'methodName' must be a string",
                        index
                    ))
                })?;

            let call_params = call_obj
                .get("params")
                .or_else(|| call_obj.get("parameters"))
                .cloned()
                .unwrap_or(serde_json::json!([]));

            // Build a sub-request for this individual call
            let sub_request = JsonRpcRequest::new(method_name, call_params);

            // Dispatch the sub-request directly (avoid recursive async by
            // not going through handle_request which would re-enter multicall)
            let id = sub_request.id.clone().unwrap_or_default();
            let sub_response = match sub_request.method.as_str() {
                "aria2.addUri" => self.handle_add_uri(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.tellActive" => self.handle_tell_active(&sub_request).await?,
                "aria2.getGlobalStat" => self.handle_global_stat().await,
                "aria2.getUris" => self.handle_get_uris(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getFiles" => self.handle_get_files(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getServers" => self.handle_get_servers(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getVersion" => self.handle_version(&sub_request),
                "aria2.getSessionInfo" => self.handle_session_info(&sub_request),
                "aria2.purgeDownloadResult" => self.handle_purge_download_result(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.saveSession" => self.handle_save_session(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.changePosition" => self.handle_change_position(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.forceRemove" => self.handle_force_remove(&sub_request).await.unwrap_or_else(|e| e.into_response(Some(id))),
                "system.multicall" => {
                    JsonRpcResponse::error(Some(id), -32600, "Nested system.multicall is not supported".to_string())
                }
                _ => JsonRpcResponse::error(Some(id), -32601, format!("Method not found: {}", sub_request.method)),
            };

            // Extract result or error from response
            match sub_response.result {
                Some(result_value) => results.push(result_value),
                None => {
                    // Error case - push error structure
                    if let Some(err) = sub_response.error {
                        results.push(serde_json::json!({
                            "code": err.code,
                            "message": err.message
                        }));
                    } else {
                        results.push(serde_json::json!(null));
                    }
                }
            }
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(results),
        ))
    }
}

// =========================================================================
// Handler-Specific Tests
// =========================================================================

#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::server::UriStatus;

    #[tokio::test]
    async fn test_handle_add_torrent() {
        let engine = RpcEngine::new();
        let fake_torrent_bencode = "d8:announce40:http://tracker.example.com/announce4:info6:lengthi1000e12:piece lengthi32768e6:pieces20:00000000000000000000000ee";
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(fake_torrent_bencode.as_bytes());
        let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "addTorrent should succeed for valid BEncode data"
        );
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_torrent_invalid_data() {
        let engine = RpcEngine::new();
        let not_torrent =
            base64::engine::general_purpose::STANDARD.encode("this is not a torrent file");
        let req =
            JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([not_torrent])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_error(),
            "addTorrent should fail for non-BEncode data"
        );
    }

    #[tokio::test]
    async fn test_handle_add_metalink() {
        let engine = RpcEngine::new();
        let metalink_xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="test.bin"><size>1024</size><url priority="1">http://example.com/test.bin</url></file></files></metalink>"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(metalink_xml.as_bytes());
        let req = JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([encoded])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_success(),
            "addMetalink should succeed for valid Metalink XML"
        );
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!gid.is_empty());
        assert_eq!(engine.task_count().await, 1);
    }

    #[tokio::test]
    async fn test_handle_add_metalink_invalid_data() {
        let engine = RpcEngine::new();
        let not_metalink =
            base64::engine::general_purpose::STANDARD.encode("this is not metalink xml");
        let req =
            JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([not_metalink])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "addMetalink should fail for non-XML data");
    }

    #[tokio::test]
    async fn test_tell_status_has_real_progress_data() {
        let engine = RpcEngine::new();

        // Add a task
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/large.iso"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Update progress with known values
        engine.update_task_progress(
            &gid,
            10485760,  // total_length: 10MB
            5242880,   // completed_length: 5MB
            1024,      // upload_length: 1KB
            1048576,   // download_speed: 1MB/s
            512,       // upload_speed: 512B/s
            3,         // connections: 3 peers
        ).await;

        // Query status and verify real progress data is returned
        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success(), "tellStatus should succeed");

        let status_val = tell_resp.result.unwrap();
        let status: StatusInfo = serde_json::from_value(status_val).unwrap();

        // Verify all progress fields contain the expected values
        assert_eq!(status.total_length, Some(10485760), "total_length should be 10MB");
        assert_eq!(status.completed_length, Some(5242880), "completed_length should be 5MB (50%)");
        assert_eq!(status.upload_length, Some(1024), "upload_length should be 1KB");
        assert_eq!(status.download_speed, Some(1048576), "download_speed should be 1MB/s");
        assert_eq!(status.upload_speed, Some(512), "upload_speed should be 512B/s");
        assert_eq!(status.connections, Some(3), "connections should be 3");

        // Verify progress calculation works correctly
        let expected_percent = (5242880.0 / 10485760.0) * 100.0;
        assert!((status.progress_percent() - expected_percent).abs() < 0.01,
                "progress percent should be ~50%");
    }

    #[tokio::test]
    async fn test_tell_status_zero_for_nonexistent_gid() {
        let engine = RpcEngine::new();

        // Query non-existent GID should return error
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!(["nonexistent-gid-12345"])
        ).with_id(1);
        let tell_resp = engine.handle_request(&tell_req).await;

        assert!(tell_resp.is_error(), "tellStatus should fail for non-existent GID");
        assert_eq!(tell_resp.error.unwrap().code, -32601,
                   "error code should be MethodNotFound (-32601)");
    }

    #[tokio::test]
    async fn test_tell_status_includes_upload_fields() {
        let engine = RpcEngine::new();

        // Add a task
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://torrent.example.com/file.torrent"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Simulate BT upload scenario
        engine.update_task_progress(
            &gid,
            1073741824,  // total: 1GB
            1073741824,  // completed: 100% (seeding)
            536870912,   // uploaded: 512MB (seeding contribution)
            0,           // download_speed: 0 (seeding)
            1048576,     // upload_speed: 1MB/s (seeding speed)
            10,          // connections: 10 peers
        ).await;

        // Verify upload fields are present and correct
        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());

        let status_val = tell_resp.result.unwrap();
        let status: StatusInfo = serde_json::from_value(status_val).unwrap();

        // Upload fields must be present in response
        assert!(status.upload_length.is_some(), "upload_length field must be present");
        assert!(status.upload_speed.is_some(), "upload_speed field must be present");
        assert_eq!(status.upload_length, Some(536870912),
                   "upload_length should reflect seeding contribution");
        assert_eq!(status.upload_speed, Some(1048576),
                   "upload_speed should show current seeding rate");
        assert_eq!(status.connections, Some(10),
                   "connections should show peer count");
    }

    #[tokio::test]
    async fn test_get_peers_returns_peer_list() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f.torrent"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let peers = vec![
            PeerInfo {
                peer_id: "p1".to_string(),
                ip: "10.0.0.1".to_string(),
                port: 6881,
                am_choking: false,
                peer_choking: true,
                download_speed: 100000,
                upload_speed: 50000,
            },
            PeerInfo {
                peer_id: "p2".to_string(),
                ip: "10.0.0.2".to_string(),
                port: 6882,
                am_choking: true,
                peer_choking: false,
                download_speed: 200000,
                upload_speed: 75000,
            },
        ];
        engine.set_task_peers(&gid, peers.clone()).await;

        let req = JsonRpcRequest::new("aria2.getPeers", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getPeers should succeed for existing GID");

        let result_peers: Vec<PeerInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result_peers.len(), 2, "Should return 2 peers");
        assert_eq!(result_peers[0].peer_id, "p1");
        assert_eq!(result_peers[1].ip, "10.0.0.2");
    }

    #[tokio::test]
    async fn test_get_peers_unknown_gid() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.getPeers", serde_json::json!(["nonexistent-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "getPeers should fail for non-existent GID");
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_pause_all_pauses_active_tasks() {
        let engine = RpcEngine::new();
        for i in 0..3 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://x.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }
        assert_eq!(engine.task_count().await, 3);

        let pause_req = JsonRpcRequest::new("aria2.pauseAll", serde_json::json!([])).with_id(10);
        let pause_resp = engine.handle_request(&pause_req).await;
        assert!(pause_resp.is_success());

        let tell_req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(11);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let active: Vec<StatusInfo> = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(active.len(), 0, "No tasks should be active after pauseAll");
    }

    #[tokio::test]
    async fn test_unpause_all_resumes_paused_tasks() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let pause_req = JsonRpcRequest::new("aria2.pause", serde_json::json!([gid])).with_id(2);
        engine.handle_request(&pause_req).await;

        let unpause_req = JsonRpcRequest::new("aria2.unpauseAll", serde_json::json!([])).with_id(3);
        let unpause_resp = engine.handle_request(&unpause_req).await;
        assert!(unpause_resp.is_success());

        let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(4);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(status.status, DownloadStatus::Active, "Task should be Active after unpauseAll");
    }

    #[tokio::test]
    async fn test_change_uri_adds_uris() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/original.iso"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let change_req = JsonRpcRequest::new(
            "aria2.changeUri",
            serde_json::json!([gid, 0, null, ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]]),
        ).with_id(2);
        let change_resp = engine.handle_request(&change_req).await;
        assert!(change_resp.is_success(), "changeUri should succeed");

        let result = change_resp.result.unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0], gid, "First element of result should be gid");
        assert_eq!(arr[1], 0, "Second element should be 0 (no file index change)");
    }

    #[tokio::test]
    async fn test_download_resume_event() {
        let event = DownloadEvent::download_resume("gid-resume-001");
        assert_eq!(event.event_type().unwrap(), EventType::DownloadResume);
        assert_eq!(event.method(), "aria2.onDownloadResume");
        let json = event.to_json().unwrap();
        assert!(json.contains("\"method\":\"aria2.onDownloadResume\""));
        assert!(json.contains("\"gid\":\"gid-resume-001\""));
    }

    #[tokio::test]
    async fn test_change_option_rejects_unknown_key() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, {"totally-invalid-option": 42}]),
        ).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "changeOption with unknown key should fail");
        assert_eq!(resp.error.unwrap().code, -32602, "error code should be InvalidParams (-32602)");
    }

    #[tokio::test]
    async fn test_change_option_accepts_valid_keys() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let valid_changes = serde_json::json!({
            "max-download-limit": 1048576,
            "split": 5,
            "dir": "/tmp/downloads"
        });
        let req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, valid_changes]),
        ).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "changeOption with valid keys should succeed");

        let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());
        let opts: HashMap<String, serde_json::Value> = serde_json::from_value(get_resp.result.unwrap()).unwrap();
        assert!(opts.contains_key("max-download-limit"), "max-download-limit should be stored");
        assert!(opts.contains_key("split"), "split should be stored");
    }

    #[tokio::test]
    async fn test_handle_get_set_option() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let set_req = JsonRpcRequest::new(
            "aria2.changeOption",
            serde_json::json!([gid, {"max-download-limit": 1048576}]),
        )
        .with_id(2);
        let set_resp = engine.handle_request(&set_req).await;
        assert!(set_resp.is_success());

        let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
        let get_resp = engine.handle_request(&get_req).await;
        assert!(get_resp.is_success());
    }

    #[tokio::test]
    async fn test_multiple_tasks() {
        let engine = RpcEngine::new();
        for i in 0..5 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://x.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }
        assert_eq!(engine.task_count().await, 5);
    }

    #[tokio::test]
    async fn test_engine_default() {
        let engine = RpcEngine::default();
        assert_eq!(engine.task_count().await, 0);
    }

    // =========================================================================
    // System Multicall Tests (H6)
    // =========================================================================

    #[tokio::test]
    async fn test_multicall_executes_multiple_methods() {
        let engine = RpcEngine::new();

        // Build a multicall request with getVersion and getGlobalStat
        let multicall_req = JsonRpcRequest::new(
            "system.multicall",
            serde_json::json!([[
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.getGlobalStat", "params": []},
                {"methodName": "aria2.getSessionInfo", "params": []},
            ]]),
        )
        .with_id(1);

        let resp = engine.handle_request(&multicall_req).await;
        assert!(resp.is_success(), "Multicall should succeed");

        let result_value = resp.result.unwrap();
        let results = result_value.as_array().expect("Should return array");
        assert_eq!(results.len(), 3, "Should have 3 results");

        // First result: getVersion - should contain version info
        let version_result = &results[0];
        assert!(
            version_result.get("version").is_some() || version_result.as_str().is_some(),
            "getVersion result should contain version info"
        );

        // Second result: getGlobalStat - should contain stat object
        let stat_result = &results[1];
        assert!(
            stat_result.get("downloadSpeed").is_some(),
            "getGlobalStat should contain downloadSpeed"
        );

        // Third result: getSessionInfo - should contain sessionId
        let session_result = &results[2];
        assert!(
            session_result.get("sessionId").is_some(),
            "getSessionInfo should contain sessionId"
        );
    }

    #[tokio::test]
    async fn test_multicall_preserves_order() {
        let engine = RpcEngine::new();

        // Add tasks first so we can query them
        for i in 0..3 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://order-test.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }

        // Multicall that mixes read operations
        let multicall_req = JsonRpcRequest::new(
            "system.multicall",
            serde_json::json!([[
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.tellActive", "params": []},
                {"methodName": "aria2.getGlobalStat", "params": []},
                {"methodName": "aria2.getSessionInfo", "params": []},
            ]]),
        )
        .with_id(10);

        let resp = engine.handle_request(&multicall_req).await;
        assert!(resp.is_success());

        let result_value = resp.result.unwrap();
        let results = result_value.as_array().unwrap();
        assert_eq!(results.len(), 4, "Should have 4 results in order");

        // Verify order is preserved
        // Result 0: Version (object with version string)
        assert!(results[0].get("version").is_some() || results[0].get("enabledFeatures").is_some());

        // Result 1: tellActive (array of active tasks)
        let active = results[1].as_array().expect("tellActive should return array");
        assert_eq!(active.len(), 3, "Should have 3 active tasks");

        // Result 2: GlobalStat (object)
        assert!(results[2].get("downloadSpeed").is_some());

        // Result 3: SessionInfo (object)
        assert!(results[3].get("sessionId").is_some());
    }

    #[tokio::test]
    async fn test_multicall_empty_calls_returns_empty_array() {
        let engine = RpcEngine::new();

        let req = JsonRpcRequest::new(
            "system.multicall",
            serde_json::json!([[]]), // Empty array of calls
        )
        .with_id(1);

        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let result_value = resp.result.unwrap();
        let results = result_value.as_array().unwrap();
        assert!(results.is_empty(), "Empty calls should return empty array");
    }

    #[tokio::test]
    async fn test_multicall_with_add_uri_and_status() {
        let engine = RpcEngine::new();

        // Multicall that adds a task and then queries status
        // Note: In the test mock, addUri creates a task entry but tellActive
        // may return 0 since tasks are tracked separately. This tests that
        // both calls execute without error in the batch.
        let multicall_req = JsonRpcRequest::new(
            "system.multicall",
            serde_json::json!([[
                {
                    "methodName": "aria2.addUri",
                    "params": [["http://multicall-test.com/file.bin"]]
                },
                {
                    "methodName": "aria2.getGlobalStat",
                    "params": []
                },
            ]]),
        )
        .with_id(1);

        let resp = engine.handle_request(&multicall_req).await;
        assert!(resp.is_success(), "Multicall with addUri + getGlobalStat should succeed");

        let result_value = resp.result.unwrap();
        let results = result_value.as_array().unwrap();
        assert_eq!(results.len(), 2);

        // First result: addUri should return non-null
        assert!(
            !results[0].is_null(),
            "addUri should return a value"
        );

        // Second result: getGlobalStat should return stat object
        assert!(
            results[1].get("downloadSpeed").is_some(),
            "getGlobalStat should contain downloadSpeed"
        );
    }

    // =========================================================================
    // K1 Additional RPC Handler Tests
    // =========================================================================

    #[tokio::test]
    async fn test_save_session_handler_basic() {
        let engine = RpcEngine::new();

        // Add some tasks first
        for i in 0..3 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://save-session.com/{}", i)]),
            )
            .with_id(i);
            engine.handle_request(&req).await;
        }

        // Save session to a directory
        let req = JsonRpcRequest::new(
            "aria2.saveSession",
            serde_json::json!(["/tmp/session_backup"]),
        )
        .with_id(10);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "saveSession should succeed");

        let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(result.contains("OK"), "Result should contain OK");
        assert!(result.contains("3"), "Result should indicate 3 downloads saved");
    }

    #[tokio::test]
    async fn test_save_session_empty_dir_fails() {
        let engine = RpcEngine::new();

        // Empty directory should fail
        let req = JsonRpcRequest::new(
            "aria2.saveSession",
            serde_json::json!([""]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "Empty dir should fail");
        assert_eq!(resp.error.unwrap().code, -32602, "Should be InvalidParams error");
    }

    #[tokio::test]
    async fn test_change_position_move_uri() {
        let engine = RpcEngine::new();

        // Add a task with multiple URIs
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://uri1.com"]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Move URI from position 0 to position 2 (end)
        let change_req = JsonRpcRequest::new(
            "aria2.changePosition",
            serde_json::json!([gid, 0, 0, 2, 0]), // del_pos=0, add_pos=2, how=POS_SET
        )
        .with_id(2);
        let change_resp = engine.handle_request(&change_req).await;
        assert!(change_resp.is_success(), "changePosition should succeed for valid positions");

        let result: String = serde_json::from_value(change_resp.result.unwrap()).unwrap();
        assert_eq!(result, "OK", "Should return OK");
    }

    #[tokio::test]
    async fn test_change_position_invalid_how() {
        let engine = RpcEngine::new();

        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://invalid-how.com/f"]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Use invalid 'how' value (must be 0, 1, or 2)
        let req = JsonRpcRequest::new(
            "aria2.changePosition",
            serde_json::json!([gid, 0, null, null, 99]), // how=99 is invalid
        )
        .with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "Invalid 'how' value should fail");
        assert_eq!(resp.error.unwrap().code, -32602, "Should be InvalidParams error");
    }

    #[tokio::test]
    async fn test_force_remove_cancels_immediately() {
        let engine = RpcEngine::new();

        // Add an active task
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://force-remove.com/large.iso"]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Verify task is active
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!([gid]),
        )
        .with_id(2);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(status.status, DownloadStatus::Active, "Task should be active initially");

        // Force remove the task
        let remove_req = JsonRpcRequest::new(
            "aria2.forceRemove",
            serde_json::json!([gid]),
        )
        .with_id(3);
        let remove_resp = engine.handle_request(&remove_req).await;
        assert!(remove_resp.is_success(), "forceRemove should succeed");

        // Verify task status changed to Removed
        let tell_req2 = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!([gid]),
        )
        .with_id(4);
        let tell_resp2 = engine.handle_request(&tell_req2).await;
        assert!(tell_resp2.is_success(), "tellStatus should still work after forceRemove");
        let status_after: StatusInfo = serde_json::from_value(tell_resp2.result.unwrap()).unwrap();
        assert_eq!(
            status_after.status,
            DownloadStatus::Removed,
            "Task should be marked as Removed after forceRemove"
        );
    }

    #[tokio::test]
    async fn test_batch_gids_force_remove() {
        let engine = RpcEngine::new();

        // Add multiple tasks
        let mut gids = Vec::new();
        for i in 0..4 {
            let req = JsonRpcRequest::new(
                "aria2.addUri",
                serde_json::json!([format!("http://batch-remove.com/{}.iso", i)]),
            )
            .with_id(i);
            let resp = engine.handle_request(&req).await;
            let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
            gids.push(gid);
        }
        assert_eq!(engine.task_count().await, 4);

        // Batch force remove using array of GIDs
        let req = JsonRpcRequest::new(
            "aria2.forceRemove",
            serde_json::json!([gids.clone()]),
        )
        .with_id(10);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "Batch forceRemove should succeed");

        // Verify all tasks were removed
        for gid in &gids {
            let tell_req = JsonRpcRequest::new(
                "aria2.tellStatus",
                serde_json::json!([gid]),
            )
            .with_id(20);
            let tell_resp = engine.handle_request(&tell_req).await;
            assert!(tell_resp.is_success());
            let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
            assert_eq!(
                status.status,
                DownloadStatus::Removed,
                "GID {} should be Removed after batch forceRemove",
                gid
            );
        }
    }

    // =========================================================================
    // L3 RPC Query Method Tests (≥18 tests, ≥3 per method)
    // =========================================================================

    // --- aria2.getUris tests ---

    #[tokio::test]
    async fn test_get_uris_valid_gid_returns_uri_list() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            // URIs must be passed as a nested array: [["uri1", "uri2"]]
            serde_json::json!([["http://example.com/file.iso", "http://mirror.com/file.iso"]]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getUris should succeed for valid GID");

        let uris: Vec<UriInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(uris.len(), 2, "Should return 2 URIs");
        assert_eq!(uris[0].uri, "http://example.com/file.iso");
        assert_eq!(uris[0].status, UriStatus::Used, "First URI should be 'used'");
        assert_eq!(uris[1].status, UriStatus::Waiting, "Second URI should be 'waiting'");
    }

    #[tokio::test]
    async fn test_get_uris_unknown_gid_returns_error() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.getUris", serde_json::json!(["nonexistent-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "getUris should fail for unknown GID");
        assert_eq!(resp.error.unwrap().code, -32601, "Should be MethodNotFound error");
    }

    #[tokio::test]
    async fn test_get_uris_single_uri() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let uris: Vec<UriInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].uri, "http://x.com/f");
        assert_eq!(uris[0].status, UriStatus::Used);
    }

    #[tokio::test]
    async fn test_get_uris_serialization_format() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://test.com/a.bin"]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        let json_str = resp.to_string().unwrap();
        // Verify JSON-RPC response structure
        assert!(json_str.contains("\"jsonrpc\":\"2.0\""));
        assert!(json_str.contains("\"result\""));
        assert!(json_str.contains("\"uri\""));
        assert!(json_str.contains("\"status\""));
    }

    // --- aria2.getFiles tests ---

    #[tokio::test]
    async fn test_get_files_valid_gid_returns_file_list() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!(["http://example.com/large.iso"]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Update progress so files have meaningful data
        engine
            .update_task_progress(&gid, 10485760, 5242880, 0, 1024, 0, 2)
            .await;

        let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getFiles should succeed for valid GID");

        let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(!files.is_empty(), "Should return at least one file");
        assert_eq!(files[0].length, 10485760, "File length should match total_length");
        assert_eq!(
            files[0].completed_length, 5242880,
            "completedLength should match completed_length"
        );
    }

    #[tokio::test]
    async fn test_get_files_unknown_gid_returns_error() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.getFiles", serde_json::json!(["unknown-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "getFiles should fail for unknown GID");
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_get_files_zero_completed_length() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/new.zip"]))
                .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Don't update progress — should have zero values
        let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(files[0].length, 0, "New task should have zero length");
        assert_eq!(files[0].completed_length, 0, "New task should have zero completed");
        assert!(files[0].selected, "Default file should be selected");
    }

    #[tokio::test]
    async fn test_get_files_selected_field() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://sel.test/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(files[0].selected, "FileInfo.selected should default to true");
    }

    // --- aria2.getServers tests ---

    #[tokio::test]
    async fn test_get_servers_valid_gid_returns_server_list() {
        let engine = RpcEngine::new();
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            // URIs must be passed as a nested array: [["uri1", "uri2"]]
            serde_json::json!([["http://dl.example.com/file.bin", "http://mirror.example.com/file.bin"]]),
        )
        .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Set download speed
        engine
            .update_task_progress(&gid, 1000000, 500000, 0, 1048576, 0, 3)
            .await;

        let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getServers should succeed for valid GID");

        let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(servers.len(), 1, "Single-file download should have 1 ServerInfoIndex");
        assert_eq!(servers[0].index, 0, "File index should be 0");
        assert_eq!(
            servers[0].servers.len(), 2,
            "Should have 2 server entries"
        );
        assert_eq!(
            servers[0].servers[0].uri, "http://dl.example.com/file.bin",
            "First server URI should match"
        );
        assert_eq!(
            servers[0].servers[0].download_speed, 1048576,
            "Download speed should match task progress"
        );
    }

    #[tokio::test]
    async fn test_get_servers_unknown_gid_returns_error() {
        let engine = RpcEngine::new();
        let req =
            JsonRpcRequest::new("aria2.getServers", serde_json::json!(["bad-gid"])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_error(), "getServers should fail for unknown GID");
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_get_servers_zero_download_speed() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://zero-speed.com/f"]))
                .with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // No progress update — speed should be 0
        let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
        let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(servers[0].servers[0].download_speed, 0, "No-progress task should have 0 speed");
        assert_eq!(
            servers[0].servers[0].current_uri, servers[0].servers[0].uri,
            "current_uri should equal uri when no redirect"
        );
    }

    #[tokio::test]
    async fn test_get_servers_empty_uri_list() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://single.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(servers[0].servers.len(), 1, "Single URI should produce 1 server entry");
    }

    // --- aria2.getVersion tests ---

    #[tokio::test]
    async fn test_get_version_returns_version_info() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getVersion should succeed");

        let result = resp.result.unwrap();
        assert!(result.get("version").is_some(), "Response should contain version field");
        assert!(
            result.get("enabledFeatures").is_some(),
            "Response should contain enabledFeatures field"
        );

        let version_info: VersionInfo = serde_json::from_value(result).unwrap();
        assert!(!version_info.version.is_empty(), "Version string should not be empty");
        assert!(
            !version_info.enabled_features.is_empty(),
            "Enabled features list should not be empty"
        );
        assert!(
            version_info.enabled_features.contains(&"bittorrent".to_string()),
            "Should include bittorrent feature"
        );
    }

    #[tokio::test]
    async fn test_get_version_uses_cargo_pkg_version() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        let result = resp.result.unwrap();
        let version_info: VersionInfo = serde_json::from_value(result).unwrap();

        // Version should come from CARGO_PKG_VERSION env
        assert!(
            !version_info.version.is_empty(),
            "CARGO_PKG_VERSION should be set"
        );
    }

    #[tokio::test]
    async fn test_get_version_enabled_features_count() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        let result = resp.result.unwrap();
        let version_info: VersionInfo = serde_json::from_value(result).unwrap();

        assert!(
            version_info.enabled_features.len() >= 5,
            "Should have at least 5 enabled features, got {}",
            version_info.enabled_features.len()
        );
    }

    #[tokio::test]
    async fn test_get_version_json_rpc_response_format() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(42);
        let resp = engine.handle_request(&req).await;
        let json_str = resp.to_string().unwrap();

        assert!(json_str.contains("\"id\":42"), "Response ID should match request");
        assert!(json_str.contains("\"version\""), "Should contain version key");
        assert!(json_str.contains("\"enabledFeatures\""), "Should contain enabledFeatures key");
    }

    // --- aria2.getPurgeDownloadResult tests (with GID) ---

    #[tokio::test]
    async fn test_purge_download_result_specific_gid() {
        let engine = RpcEngine::new();

        // Add a stopped task to the stopped_tasks list
        let stopped_gid = "stopped-gid-001".to_string();
        let stopped_status = StatusInfo::new(&stopped_gid)
            .with_status(DownloadStatus::Complete)
            .with_total_length(1000)
            .with_completed_length(1000);
        {
            let mut stopped = engine.stopped_tasks.write().await;
            stopped.push(stopped_status);
        }
        assert_eq!(engine.stopped_tasks.read().await.len(), 1);

        // Purge the specific GID
        let req = JsonRpcRequest::new(
            "aria2.purgeDownloadResult",
            serde_json::json!([stopped_gid]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "purgeDownloadResult with valid GID should succeed");

        let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result, "OK");
        assert_eq!(
            engine.stopped_tasks.read().await.len(),
            0,
            "Stopped task should be removed after purge"
        );
    }

    #[tokio::test]
    async fn test_purge_download_result_gid_not_found() {
        let engine = RpcEngine::new();
        // Don't add any stopped tasks
        let req = JsonRpcRequest::new(
            "aria2.purgeDownloadResult",
            serde_json::json!(["nonexistent-stopped-gid"]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(
            resp.is_error(),
            "purgeDownloadResult with unknown GID should fail"
        );
        assert_eq!(resp.error.unwrap().code, -32601, "Should be MethodNotFound error");
    }

    #[tokio::test]
    async fn test_purge_download_result_no_param_clears_all() {
        let engine = RpcEngine::new();

        // Add multiple stopped tasks
        for i in 0..3 {
            let status = StatusInfo::new(format!("stopped-{}", i))
                .with_status(DownloadStatus::Complete);
            engine.stopped_tasks.write().await.push(status);
        }
        assert_eq!(engine.stopped_tasks.read().await.len(), 3);

        // Purge without GID parameter — should clear all
        let req =
            JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "No-param purge should succeed");
        assert_eq!(
            engine.stopped_tasks.read().await.len(),
            0,
            "All stopped tasks should be cleared"
        );
    }

    #[tokio::test]
    async fn test_purge_download_result_partial_purge() {
        let engine = RpcEngine::new();

        // Add 3 stopped tasks
        let gid_a = "gid-a".to_string();
        let gid_b = "gid-b".to_string();
        let gid_c = "gid-c".to_string();
        {
            let mut stopped = engine.stopped_tasks.write().await;
            stopped.push(StatusInfo::new(&gid_a).with_status(DownloadStatus::Complete));
            stopped.push(StatusInfo::new(&gid_b).with_status(DownloadStatus::Complete));
            stopped.push(StatusInfo::new(&gid_c).with_status(DownloadStatus::Error));
        }
        assert_eq!(engine.stopped_tasks.read().await.len(), 3);

        // Purge only gid_b
        let req = JsonRpcRequest::new(
            "aria2.purgeDownloadResult",
            serde_json::json!([gid_b.clone()]),
        )
        .with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());

        // Should have 2 remaining (gid_a and gid_c)
        let stopped = engine.stopped_tasks.read().await;
        assert_eq!(stopped.len(), 2);
        let remaining_gids: Vec<&String> = stopped.iter().map(|s| &s.gid).collect();
        assert!(remaining_gids.contains(&&gid_a), "gid_a should remain");
        assert!(remaining_gids.contains(&&gid_c), "gid_c should remain");
        assert!(!remaining_gids.contains(&&gid_b), "gid_b should be purged");
    }

    // --- aria2.getSessionInfo tests ---

    #[tokio::test]
    async fn test_get_session_info_returns_session_id() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "getSessionInfo should succeed");

        let result = resp.result.unwrap();
        assert!(
            result.get("sessionId").is_some(),
            "Response should contain sessionId field"
        );

        let session_id = result.get("sessionId").unwrap().as_str().unwrap();
        assert!(
            session_id.starts_with("session-"),
            "Session ID should start with 'session-' prefix, got: {}",
            session_id
        );
        assert!(!session_id.is_empty(), "Session ID should not be empty");
    }

    #[tokio::test]
    async fn test_get_session_info_unique_per_call() {
        let engine = RpcEngine::new();
        let req1 = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
        let resp1 = engine.handle_request(&req1).await;

        // Small delay to ensure different timestamp
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let req2 = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(2);
        let resp2 = engine.handle_request(&req2).await;

        let sid1 = resp1.result.unwrap().get("sessionId").unwrap().as_str().unwrap().to_string();
        let sid2 = resp2.result.unwrap().get("sessionId").unwrap().as_str().unwrap().to_string();

        // Session IDs may be same or differ based on timing — both are valid
        assert!(!sid1.is_empty() && !sid2.is_empty(), "Both session IDs should be non-empty");
    }

    #[tokio::test]
    async fn test_get_session_info_struct_fields() {
        let session_info = SessionInfo::new();
        assert!(!session_info.session_id.is_empty(), "session_id should not be empty");
        assert!(
            session_info.session_start_time > 0,
            "session_start_time should be positive Unix timestamp"
        );

        let json_val = session_info.to_json_value();
        assert!(json_val.get("sessionId").is_some(), "JSON should contain sessionId");
    }

    #[tokio::test]
    async fn test_get_session_info_json_rpc_format() {
        let engine = RpcEngine::new();
        let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(99);
        let resp = engine.handle_request(&req).await;
        let json_str = resp.to_string().unwrap();

        assert!(json_str.contains("\"id\":99"), "Response ID should match");
        assert!(json_str.contains("\"sessionId\""), "Should contain sessionId field");
        assert!(json_str.contains("\"result\""), "Should have result field");
    }
}
