//! BitTorrent and utility RPC handlers.
//!
//! Handlers for BT-specific operations, bulk operations, L3 query methods,
//! and system/multicall support.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{
    DownloadStatus, FileInfo, ServerInfo, ServerInfoIndex, UriEntry, VersionInfo,
};
use crate::websocket::{DownloadEvent, EventType};

impl RpcEngine {
    /// Handle `aria2.removeDownloadResult` - Remove a specific stopped download result.
    pub async fn handle_remove_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
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

    /// Handle `aria2.getPeers` - Get peer list for a BitTorrent download.
    pub async fn handle_get_peers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(&state.peers).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.pauseAll` - Pause all active downloads.
    pub async fn handle_pause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Active {
                state.status.status = DownloadStatus::Paused;
                let _ = self.event_publisher.publish(
                    EventType::DownloadPause,
                    DownloadEvent::download_pause(&state.status.gid),
                );
            }
        }
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!("OK"))
    }

    /// Handle `aria2.forcePauseAll` - Force pause all active downloads.
    pub async fn handle_force_pause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let mut tasks_map = self.tasks.write().await;

        for task_state in tasks_map.values_mut() {
            if task_state.status.status == DownloadStatus::Active {
                task_state.status.status = DownloadStatus::Paused;
                if let Some(cancel_token) = &task_state.cancel_token {
                    cancel_token.cancel();
                }
            }
        }

        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!("OK"))
    }

    /// Handle `aria2.unpauseAll` - Resume all paused downloads.
    pub async fn handle_unpause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Paused {
                state.status.status = DownloadStatus::Active;
            }
        }
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), serde_json::json!("OK"))
    }

    /// Handle `aria2.getUris` - Get URI list for a download with status.
    pub async fn handle_get_uris(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                let uris: Vec<UriEntry> = state
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
                    serde_json::to_value(uris).map_err(|e| {
                        JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                    })?,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getFiles` - Get file list for a download.
    pub async fn handle_get_files(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                let files = match &state.status.files {
                    Some(files_vec) if !files_vec.is_empty() => files_vec
                        .iter()
                        .enumerate()
                        .map(|(i, f)| FileInfo {
                            index: i,
                            path: f.path.clone(),
                            length: if f.length == 0 {
                                state.total_length
                            } else {
                                f.length
                            },
                            completed_length: if f.completed_length == 0 {
                                state.completed_length
                            } else {
                                f.completed_length
                            },
                            selected: f.selected,
                            uris: f.uris.clone(),
                        })
                        .collect(),
                    _ => {
                        vec![
                            FileInfo::new(
                                state.uris.first().map(|s| s.as_str()).unwrap_or(""),
                                state.total_length,
                            )
                            .with_completed(state.completed_length),
                        ]
                    }
                };
                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(files).map_err(|e| {
                        JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                    })?,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getServers` - Get active server connection information.
    pub async fn handle_get_servers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        match tasks.get(&gid) {
            Some(state) => {
                let servers: Vec<ServerInfo> = state
                    .uris
                    .iter()
                    .map(|u| ServerInfo::new(u.as_str()).with_download_speed(state.download_speed))
                    .collect();

                let result = vec![ServerInfoIndex { index: 0, servers }];

                Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(result).map_err(|e| {
                        JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                    })?,
                ))
            }
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.getVersion` - Get version information with enabled features.
    pub fn handle_version(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let version_info = VersionInfo::from_env();
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            version_info.to_json_value(),
        )
    }

    /// Handle `aria2.getPurgeDownloadResult` - Purge download results.
    pub async fn handle_purge_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        match req.get_param::<String>(0) {
            Ok(gid) => {
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
    /// Returns the same `sessionId` for every call within a session, matching
    /// C++ aria2 which generates `sessionId_` once at engine construction.
    pub fn handle_session_info(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            self.session_info.to_json_value(),
        )
    }

    /// Handle `system.multicall` - Execute multiple RPC calls in one HTTP request.
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

            let sub_request = JsonRpcRequest::new(method_name, call_params);

            let id = sub_request.id.clone().unwrap_or_default();
            let sub_response = match sub_request.method.as_str() {
                "aria2.addUri" => self
                    .handle_add_uri(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.tellActive" => self.handle_tell_active(&sub_request).await?,
                "aria2.getGlobalStat" => self.handle_global_stat(&sub_request).await,
                "aria2.getUris" => self
                    .handle_get_uris(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getFiles" => self
                    .handle_get_files(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getServers" => self
                    .handle_get_servers(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getVersion" => self.handle_version(&sub_request),
                "aria2.getSessionInfo" => self.handle_session_info(&sub_request),
                "aria2.purgeDownloadResult" => self
                    .handle_purge_download_result(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.saveSession" => self
                    .handle_save_session(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.changePosition" => self
                    .handle_change_position(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.forceRemove" => self
                    .handle_force_remove(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "system.multicall" => JsonRpcResponse::error(
                    Some(id),
                    -32600,
                    "Nested system.multicall is not supported".to_string(),
                ),
                _ => JsonRpcResponse::error(
                    Some(id),
                    -32601,
                    format!("Method not found: {}", sub_request.method),
                ),
            };

            // Per C++ aria2 system.multicall spec: each successful result is
            // wrapped in an extra array layer so the output is [[result]], not
            // [result]. Errors remain as flat structs {code, message}.
            match sub_response.result {
                Some(result_value) => results.push(serde_json::json!([result_value])),
                None => {
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
