//! BitTorrent and utility RPC handlers.
//!
//! Handlers for BT-specific operations, bulk operations, L3 query methods,
//! and system/multicall support.

use crate::engine::{RpcEngine, rpc_method_requires_auth};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::rpc_helpers::split_auth_token;
use crate::types::{DownloadStatus, PeerInfo, ServerInfo, ServerInfoIndex, UriEntry, VersionInfo};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::util::rwlock_ext::RwLockRecover;

impl RpcEngine {
    async fn lifecycle_gids(&self) -> Vec<String> {
        let Some(group_man) = &self.group_man else {
            return Vec::new();
        };
        let man = group_man;
        man.all_groups()
            .into_iter()
            .map(|(_, group)| group.recover().gid().to_hex_string())
            .collect()
    }

    /// Handle `aria2.removeDownloadResult` - Remove a specific stopped download result.
    pub async fn handle_remove_download_result(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let group_man = self.group_man.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.removeDownloadResult is not supported by the core state model".into(),
            )
        })?;
        let man = group_man;
        if man.remove_stopped_result(&gid).is_some() {
            Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                "OK",
            ))
        } else {
            Err(JsonRpcError::RpcExecution(format!(
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
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man;
        let group = man
            .group_by_hex(&gid)
            .ok_or_else(|| JsonRpcError::RpcExecution(format!("GID {} not found", gid)))?;
        let peers = group
            .recover()
            .bt_peer_snapshots()
            .into_iter()
            .map(|peer| PeerInfo {
                peer_id: peer
                    .peer_id
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                ip: peer.addr.ip().to_string(),
                port: peer.addr.port(),
                bitfield: None,
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                download_speed: peer.download_speed.max(0.0) as u64,
                upload_speed: peer.upload_speed.max(0.0) as u64,
                seeder: peer.seeder.map(|value| value.to_string()),
            })
            .collect::<Vec<_>>();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(peers).map_err(|error| {
                JsonRpcError::InternalError(format!("Serialization failed: {error}"))
            })?,
        ))
    }

    /// Handle `aria2.pauseAll` - Pause all active downloads.
    pub async fn handle_pause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let gids = self.lifecycle_gids().await;
        if let Some(group_man) = &self.group_man {
            group_man.pause_all();
        }
        let result = self
            .engine_cmd_tx
            .as_ref()
            .ok_or_else(|| {
                JsonRpcError::RpcExecution(
                    "aria2.pauseAll is not supported by the core state model".into(),
                )
            })
            .and_then(|tx| {
                tx.send(EngineCommand::PauseAll).map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                })
            });
        match result {
            Ok(()) => {
                for gid in gids {
                    let _ = self
                        .event_publisher
                        .publish(EventType::DownloadPause, DownloadEvent::download_pause(gid));
                }
                JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!("OK"),
                )
            }
            Err(e) => e.into_response(req.id.clone()),
        }
    }

    /// Handle `aria2.forcePauseAll` - Force pause all active downloads.
    pub async fn handle_force_pause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let gids = self.lifecycle_gids().await;
        if let Some(group_man) = &self.group_man {
            group_man.force_pause_all();
        }
        let result = self
            .engine_cmd_tx
            .as_ref()
            .ok_or_else(|| {
                JsonRpcError::RpcExecution(
                    "aria2.forcePauseAll is not supported by the core state model".into(),
                )
            })
            .and_then(|tx| {
                tx.send(EngineCommand::ForcePauseAll).map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                })
            });
        match result {
            Ok(()) => {
                for gid in gids {
                    let _ = self
                        .event_publisher
                        .publish(EventType::DownloadPause, DownloadEvent::download_pause(gid));
                }
                JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!("OK"),
                )
            }
            Err(e) => e.into_response(req.id.clone()),
        }
    }

    /// Handle `aria2.unpauseAll` - Resume all paused downloads.
    pub async fn handle_unpause_all(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let gids = self.lifecycle_gids().await;
        if let Some(group_man) = &self.group_man {
            group_man.unpause_all();
        }
        let result = self
            .engine_cmd_tx
            .as_ref()
            .ok_or_else(|| {
                JsonRpcError::RpcExecution(
                    "aria2.unpauseAll is not supported by the core state model".into(),
                )
            })
            .and_then(|tx| {
                tx.send(EngineCommand::UnpauseAll).map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                })
            });
        match result {
            Ok(()) => {
                for gid in gids {
                    let _ = self
                        .event_publisher
                        .publish(EventType::DownloadStart, DownloadEvent::download_start(gid));
                }
                JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::json!("OK"),
                )
            }
            Err(e) => e.into_response(req.id.clone()),
        }
    }

    /// Handle `aria2.getUris` - Get URI list for a download with status.
    pub async fn handle_get_uris(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man;
        let group = man
            .group_by_hex(&gid)
            .ok_or_else(|| JsonRpcError::RpcExecution(format!("GID {} not found", gid)))?;
        let guard = group.recover();
        let entries = guard
            .get_download_context()
            .and_then(|context| {
                context
                    .get_file_entries()
                    .first()
                    .map(RpcEngine::build_uri_entries)
            })
            .unwrap_or_else(|| guard.uris().iter().cloned().map(UriEntry::new).collect());
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(entries)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {e}")))?,
        ))
    }

    /// Handle `aria2.getFiles` - Get file list for a download.
    pub async fn handle_get_files(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man;
        let files = if let Some(group) = man.group_by_hex(&gid) {
            let guard = group.recover();
            let completed = guard.get_completed_length();
            RpcEngine::build_file_infos(&guard, completed)
        } else if let Some(result) = man.find_stopped_result(&gid) {
            RpcEngine::build_file_infos_from_result(&result)
        } else {
            return Err(JsonRpcError::RpcExecution(format!(
                "No file data is available for GID#{}",
                gid
            )));
        };
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(files)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {e}")))?,
        ))
    }

    /// Handle `aria2.getServers` - Get active server connection information.
    pub async fn handle_get_servers(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man;
        let group = man.group_by_hex(&gid).ok_or_else(|| {
            JsonRpcError::RpcExecution(format!("No active download for GID#{}", gid))
        })?;
        let guard = group.recover();
        if !matches!(guard.status(), DownloadStatus::Active) {
            return Err(JsonRpcError::RpcExecution(format!(
                "No active download for GID#{}",
                gid
            )));
        }
        let files = guard
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
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(files)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {e}")))?,
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
        let group_man = self.group_man.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.purgeDownloadResult is not supported by the core state model".into(),
            )
        })?;
        // The original method has no parameters and purges all retained
        // results. It intentionally ignores the request object entirely.
        group_man.purge_stopped_results();
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
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
    ///
    /// Every sub-call is routed through [`RpcEngine::dispatch_single`] — the
    /// exact same method table `handle_request` uses — so the batched API
    /// surface always matches the single-call API surface. AriaNg and
    /// webui-aria2 batch their whole refresh loop (`tellActive` +
    /// `tellWaiting` + `tellStopped` + `getGlobalStat`) into one multicall,
    /// so any method missing here shows up as missing data in the UI.
    ///
    /// # Authorization
    ///
    /// Follows C++ aria2 (`SystemMulticallRpcMethod::execute` →
    /// `RpcMethod::execute` → `RpcMethod::authorize`): the multicall envelope
    /// itself is not authorized, but **each sub-call is**. A sub-call carries
    /// its own `"token:xxx"` first parameter, which is validated and then
    /// stripped so the handler's positional arguments do not shift. The
    /// envelope must contain the call list at parameter zero; an envelope
    /// token is not a supported alternative in aria2's wire contract.
    ///
    /// # Error handling
    ///
    /// A failing sub-call never aborts the batch: its error is reported in
    /// place and the loop continues, mirroring C++ which appends
    /// `createErrorResponse(...)` and moves on to the next entry.
    pub async fn handle_multicall(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // C++ checkRequiredParam<List>() reports a method execution error
        // (code 1), rather than JSON-RPC -32602, for a missing or mistyped
        // multicall envelope parameter.
        let calls: Vec<serde_json::Value> = match req.optional_param_value(0) {
            Some(serde_json::Value::Array(calls)) => calls.clone(),
            Some(_) => {
                return Err(JsonRpcError::RpcExecution(
                    "The parameter at 0 has wrong type.".to_string(),
                ));
            }
            None => {
                return Err(JsonRpcError::RpcExecution(
                    "The parameter at 0 is required but missing.".to_string(),
                ));
            }
        };

        if calls.is_empty() {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::json!([]),
            ));
        }

        let mut results = Vec::with_capacity(calls.len());

        for call_obj in &calls {
            let Some(call_obj) = call_obj.as_object() else {
                results.push(serde_json::json!({
                    "code": 1,
                    "message": "system.multicall expected struct."
                }));
                continue;
            };
            let Some(method_name) = call_obj.get("methodName").and_then(|value| value.as_str())
            else {
                results.push(serde_json::json!({
                    "code": 1,
                    "message": "Missing methodName."
                }));
                continue;
            };
            if method_name == "system.multicall" {
                results.push(serde_json::json!({
                    "code": 1,
                    "message": "Recursive system.multicall forbidden."
                }));
                continue;
            }

            // The original implementation accepts only a list for a
            // sub-call's params member. Missing, null, object, and scalar
            // values all become an empty list before authorization/dispatch.
            let call_params = match call_obj.get("params") {
                Some(serde_json::Value::Array(params)) => serde_json::Value::Array(params.clone()),
                _ => serde_json::Value::Array(Vec::new()),
            };

            // Authorize the sub-call the same way C++ RpcMethod::authorize()
            // does for every method invocation: pop a leading "token:xxx"
            // parameter and validate it. Without this the token would leak
            // into the handler as a positional argument and shift every
            // subsequent parameter by one.
            let (sub_token, stripped_params) = split_auth_token(&call_params);
            let sub_request =
                JsonRpcRequest::new(method_name, stripped_params.unwrap_or(call_params));

            let sub_response = if rpc_method_requires_auth(method_name) {
                match self.auth_middleware.validate(sub_token.as_deref()) {
                    Ok(()) => self.dispatch_single(&sub_request).await,
                    Err(auth_err) => auth_err.into_response(sub_request.id.clone()),
                }
            } else {
                self.dispatch_single(&sub_request).await
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
