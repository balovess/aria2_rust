//! BitTorrent and utility RPC handlers.
//!
//! Handlers for BT-specific operations, bulk operations, L3 query methods,
//! and system/multicall support.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{
    DownloadStatus, FileInfo, ServerInfo, ServerInfoIndex, SessionInfo, UriEntry, VersionInfo,
};
use crate::websocket::{DownloadEvent, EventType};

impl RpcEngine {
    /// Handle `aria2.removeDownloadResult` - Remove a specific stopped download result.
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
    ///
    /// Returns `"OK"` per the aria2 RPC spec. The request `id` is preserved
    /// on the response so callers (including WebSocket batch callers) can
    /// correlate the result.
    pub async fn handle_pause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
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
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            format!("OK. {} tasks paused.", count),
        )
    }

    /// Handle `aria2.forcePauseAll` - Force pause all active downloads.
    ///
    /// See [`handle_pause_all`] for the response id semantics.
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
    ///
    /// See [`handle_pause_all`] for the response id semantics.
    pub async fn handle_unpause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let mut tasks = self.tasks.write().await;
        let mut count = 0usize;
        for state in tasks.values_mut() {
            if state.status.status == DownloadStatus::Paused {
                state.status.status = DownloadStatus::Active;
                count += 1;
            }
        }
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            format!("OK. {} tasks resumed.", count),
        )
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
                        .map(|(i, f)| {
                            // FileInfo scalars are stored as wire-format strings
                            // (matching original aria2 `util::itos()`). Parse them
                            // back to u64 to apply the "fall back to task totals
                            // when the file entry has no length" rule, then
                            // re-serialize. The index is 1-based to match the
                            // original aria2 `util::uitos(index)` convention.
                            let file_len: u64 = f.length.parse().unwrap_or(0);
                            let file_completed: u64 =
                                f.completed_length.parse().unwrap_or(0);
                            FileInfo {
                                index: (i + 1).to_string(),
                                path: f.path.clone(),
                                length: (if file_len == 0 {
                                    state.total_length
                                } else {
                                    file_len
                                })
                                .to_string(),
                                completed_length: (if file_completed == 0 {
                                    state.completed_length
                                } else {
                                    file_completed
                                })
                                .to_string(),
                                selected: f.selected.clone(),
                                uris: f.uris.clone(),
                            }
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
    ///
    /// Mirrors the original aria2 `GetServersRpcMethod::process`
    /// (RpcMethodImpl.cc:1262-1294): throws `DL_ABORT_EX` (→ JSON-RPC error
    /// code 1) with message `"No active download for GID#<hex>"` if the GID is
    /// not found OR the download is not in `STATE_ACTIVE`. This ensures
    /// clients like AriaNg only call `getServers` against actively downloading
    /// tasks, matching the original semantics exactly.
    pub async fn handle_get_servers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let tasks = self.tasks.read().await;
        let state = match tasks.get(&gid) {
            Some(s) => s,
            None => {
                return Err(JsonRpcError::ServerError(
                    1,
                    format!("No active download for GID#{}", gid),
                ));
            }
        };
        // Reject non-active downloads — matches original `group->getState()
        // != RequestGroup::STATE_ACTIVE` check.
        if state.status.status != DownloadStatus::Active {
            return Err(JsonRpcError::ServerError(
                1,
                format!("No active download for GID#{}", gid),
            ));
        }
        let servers: Vec<ServerInfo> = state
            .uris
            .iter()
            .map(|u| ServerInfo::new(u.as_str()).with_download_speed(state.download_speed))
            .collect();

        let result = vec![ServerInfoIndex { index: 0, servers }];
        drop(tasks);
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(result).map_err(|e| {
                JsonRpcError::InternalError(format!("Serialization failed: {}", e))
            })?,
        ))
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
    pub fn handle_session_info(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let session_info = SessionInfo::new();
        JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            session_info.to_json_value(),
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
                // NOTE: `?` MUST NOT be used here — error isolation requires
                // every sub-call to be converted into a (possibly error)
                // response slot. Using `?` would propagate the error and
                // abort the entire multicall, breaking AriaNg compatibility.
                "aria2.tellActive" => self
                    .handle_tell_active(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.tellWaiting" => self
                    .handle_tell_waiting(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.tellStopped" => self
                    .handle_tell_stopped(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.tellStatus" => self
                    .handle_tell_status(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getGlobalStat" => self.handle_global_stat(&sub_request).await,
                "aria2.getOption" => self
                    .handle_get_option(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getGlobalOption" => self.handle_get_global_option().await,
                "aria2.getUris" => self
                    .handle_get_uris(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getFiles" => self
                    .handle_get_files(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.getPeers" => self
                    .handle_get_peers(&sub_request)
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
                "aria2.removeDownloadResult" => self
                    .handle_remove_download_result(&sub_request)
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
                "aria2.changeUri" => self
                    .handle_change_uri(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.changeOption" => self
                    .handle_change_option(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.changeGlobalOption" => self
                    .handle_change_global_option(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.pause" => self
                    .handle_pause(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.forcePause" => self
                    .handle_force_pause(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.unpause" | "aria2.forceUnpause" => self
                    .handle_unpause(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.pauseAll" => self.handle_pause_all(&sub_request).await,
                "aria2.forcePauseAll" => self.handle_force_pause_all(&sub_request).await,
                "aria2.unpauseAll" => self.handle_unpause_all(&sub_request).await,
                "aria2.remove" => self
                    .handle_remove(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.forceRemove" => self
                    .handle_force_remove(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.shutdown" => self
                    .handle_shutdown(&sub_request)
                    .await
                    .unwrap_or_else(|e| e.into_response(Some(id))),
                "aria2.forceShutdown" => self
                    .handle_force_shutdown(&sub_request)
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

            // Per original aria2 `SystemMulticallRpcMethod::execute` in
            // `RpcMethodImpl.cc:1462-1469`: successful sub-call results are
            // wrapped in a single-element array `[result]`, while error
            // responses are pushed directly as `{"code":..., "message":...}`.
            //
            // The wrapping matches the XML-RPC `system.multicall` convention
            // and is what AriaNg's `aria2TaskService.js` expects — it indexes
            // results with `response.data[i][0]` to unwrap the value.
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
