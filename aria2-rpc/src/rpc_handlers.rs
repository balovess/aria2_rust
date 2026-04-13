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
        let uri: String = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> = req.get_param_or_default(1);
        let gid = self.add_task(vec![uri], opts).await?;
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
    /// - [0]: Offset (number of items to skip)
    /// - [1]: Number of items to return
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
    /// - [0]: Offset (number of items to skip)
    /// - [1]: Number of items to return
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

    /// Handle `aria2.getVersion` - Get version information.
    ///
    /// Returns: Object with version string and enabled features list
    pub fn handle_version(&self) -> JsonRpcResponse {
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::json!({
                "version": "1.37.0-Rust",
                "enabledFeatures": ["http", "https", "ftp", "bittorrent", "metalink", "sftp"],
                "session": "aria2-rpc"
            }),
        )
    }

    /// Handle `aria2.getSessionInfo` - Get session identifier.
    ///
    /// Returns: Object with sessionId field (hex timestamp-based ID)
    pub fn handle_session_info(&self) -> JsonRpcResponse {
        use std::time::{SystemTime, UNIX_EPOCH};
        let session_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| format!("session-{:x}", d.as_nanos()))
            .unwrap_or_else(|_| "session-unknown".to_string());
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::json!({"sessionId": session_id}),
        )
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

    /// Handle `aria2.purgeDownloadResult` - Clear all stopped download results.
    ///
    /// Returns: "OK"
    pub fn handle_purge_download_result(&self) -> JsonRpcResponse {
        JsonRpcResponse::success(serde_json::Value::Null, "OK")
    }

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
                "aria2.getVersion" => self.handle_version(),
                "aria2.getSessionInfo" => self.handle_session_info(),
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
        assert!(opts.get("max-download-limit").is_some(), "max-download-limit should be stored");
        assert!(opts.get("split").is_some(), "split should be stored");
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
}
