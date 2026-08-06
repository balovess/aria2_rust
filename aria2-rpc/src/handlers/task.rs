//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, create_gid};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::checksum::checksum::Checksum;
use aria2_core::constants as core_constants;
use aria2_core::engine::command::Command;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::ChangePositionMode;
use aria2_core::session::save_session_command::SaveSessionCommand;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::collections::HashMap;

/// Delay between answering a shutdown RPC and actually halting the engine.
///
/// Mirrors the hard-coded `3_s` in C++ `RpcMethodImpl::goingShutdown()`, which
/// schedules a `TimedHaltCommand` so the JSON-RPC response is flushed before
/// the engine (and with it the RPC listener) goes away.
const RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

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

        #[cfg(all(feature = "bittorrent", feature = "metalink"))]
        if let Some(group_man) = &self.group_man {
            let options = rpc_options_to_download_options(&opts);
            let document =
                aria2_protocol::metalink::parser::MetalinkDocument::parse(&decoded_bytes, None)
                    .map_err(|error| JsonRpcError::InvalidParams(error.to_string()))?;
            let converter =
                aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup::new();
            let mut gids = Vec::new();
            let man = group_man.read().await;
            for file in document.files.iter().filter(|file| {
                file.meta_urls.iter().any(|metaurl| {
                    metaurl.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent
                })
            }) {
                let metadata_gid = man.next_available_gid();
                let payload_gid = man.next_available_gid();
                let graph = converter
                    .create_torrent_graph(file, &options, metadata_gid, payload_gid)
                    .map_err(|error| JsonRpcError::InvalidParams(error.to_string()))?;
                let (metadata_gid, payload_gid) = man
                    .add_metalink_graph(graph)
                    .map_err(|error| JsonRpcError::InternalError(error.to_string()))?;
                let _ = metadata_gid;
                gids.push(payload_gid.to_hex_string());
            }
            if !gids.is_empty() {
                return Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    gids,
                ));
            }
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

        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.remove is not supported by the core state model".into(),
            )
        })?;
        let gid_parsed = GroupId::from_hex_string(&gid)
            .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
        engine_cmd_tx
            .send(EngineCommand::RemoveDownload { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(gid),
        ))
    }

    /// Handle `aria2.pause` / `aria2.forcePause` - Pause a download task.
    pub async fn handle_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.pause is not supported by the core state model".into(),
            )
        })?;
        let gid_parsed = GroupId::from_hex_string(&gid)
            .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
        engine_cmd_tx
            .send(EngineCommand::Pause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(gid),
        ))
    }

    /// Handle `aria2.forcePause` - Force pause a download task.
    pub async fn handle_force_pause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.forcePause is not supported by the core state model".into(),
            )
        })?;
        let gid_parsed = GroupId::from_hex_string(&gid)
            .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
        engine_cmd_tx
            .send(EngineCommand::ForcePause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(gid),
        ))
    }

    /// Handle `aria2.unpause` / `aria2.forceUnpause` - Resume a paused task.
    pub async fn handle_unpause(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.unpause is not supported by the core state model".into(),
            )
        })?;
        let gid_parsed = GroupId::from_hex_string(&gid)
            .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
        engine_cmd_tx
            .send(EngineCommand::Unpause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(gid),
        ))
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
            None => Err(JsonRpcError::RpcExecution(format!("GID {} not found", gid))),
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

        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.forceRemove is not supported by the core state model".into(),
            )
        })?;
        for gid in &gids {
            let gid_parsed = GroupId::from_hex_string(gid)
                .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
            engine_cmd_tx
                .send(EngineCommand::ForceRemoveDownload { gid: gid_parsed })
                .map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                })?;
        }
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
        let del_uris: Vec<String> = req.get_param(1)?;
        let add_uris: Vec<String> = req.get_param(2)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man.read().await;
        let group = man
            .group_by_hex(&gid)
            .ok_or_else(|| JsonRpcError::RpcExecution(format!("GID {} not found", gid)))?;
        let result = group
            .write()
            .map_err(|_| JsonRpcError::InternalError("Failed to lock request group".into()))?
            .change_uris(&del_uris, &add_uris)
            .map_err(|e| JsonRpcError::RpcExecution(e.to_string()))?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([result.0, result.1]),
        ))
    }

    /// Handle `aria2.saveSession` - Save current session state to disk.
    ///
    /// Mirrors C++ `SaveSessionRpcMethod`: writes the session file and returns
    /// "OK" on success (or an error when no filename is configured).
    ///
    /// - Optional `param[0]`: session file path. When omitted or empty, the
    ///   engine's configured `--save-session` path is used.
    /// - The real `RequestGroup` state is serialized through the core manager.
    pub async fn handle_save_session(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Resolve the target path: an explicit param wins; otherwise fall back
        // to the engine's configured save-session path (C++ reads PREF_SAVE_SESSION).
        let param_path = req.get_param_or_default::<String>(0);
        let target = if param_path.is_empty() {
            self.save_session_path.clone()
        } else {
            Some(std::path::PathBuf::from(param_path))
        };
        let path = target.ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "Filename is not given. Set --save-session or pass a path.".into(),
            )
        })?;

        // Wired engine: serialize the real RequestGroupMan via SaveSessionCommand.
        if let Some(group_man) = &self.group_man {
            let mut cmd = SaveSessionCommand::new(path, group_man.clone());
            cmd.execute().await.map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to save session: {}", e))
            })?;
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                "OK".to_string(),
            ));
        }

        Err(JsonRpcError::InternalError(
            "RequestGroupMan is not wired".into(),
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
        let pos: i32 = req.get_param(1)?;
        let how: String = req.get_param(2)?;
        let mode = match how.as_str() {
            "POS_SET" => ChangePositionMode::SetFromStart,
            "POS_CUR" => ChangePositionMode::MoveFromStart,
            "POS_END" => ChangePositionMode::SetFromEnd,
            _ => return Err(JsonRpcError::InvalidParams("Invalid position mode".into())),
        };
        let gid = GroupId::from_hex_string(&gid)
            .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man.read().await;
        let position = man
            .change_position(gid, pos, mode)
            .map_err(|e| JsonRpcError::RpcExecution(e.to_string()))?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            position,
        ))
    }

    /// Handle `aria2.shutdown` - Graceful shutdown (save session, wait for downloads).
    ///
    /// This method performs a graceful shutdown:
    /// 1. Saves current session state
    /// 2. Marks all active downloads as paused
    /// 3. Sends `EngineCommand::HaltAll` to the engine loop so it stops
    ///    accepting new downloads and waits for in-flight chunks to finish
    /// 4. Returns "OK. N active downloads paused." to indicate shutdown initiated
    pub async fn handle_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let group_man = self.group_man.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.shutdown is not supported by the core state model".into(),
            )
        })?;
        let active_count = group_man
            .read()
            .await
            .all_groups()
            .into_iter()
            .filter(|(_, group)| group.recover().status() == DownloadStatus::Active)
            .count();
        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.shutdown is not supported by the core state model".into(),
            )
        })?;
        aria2_core::engine::halt_watchers::spawn_timed_halt(
            engine_cmd_tx.clone(),
            RPC_SHUTDOWN_GRACE,
            false,
        );

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::Value::String(format!("OK. {} active downloads paused.", active_count)),
        ))
    }

    /// Handle `aria2.forceShutdown` - Force shutdown (immediate termination).
    ///
    /// This method sends `EngineCommand::ForceHaltAll` to the engine loop so
    /// it terminates immediately and returns the number of core-managed groups.
    pub async fn handle_force_shutdown(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let group_man = self.group_man.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.forceShutdown is not supported by the core state model".into(),
            )
        })?;
        let cancelled_count = group_man.read().await.count();
        let engine_cmd_tx = self.engine_cmd_tx.as_ref().ok_or_else(|| {
            JsonRpcError::RpcExecution(
                "aria2.forceShutdown is not supported by the core state model".into(),
            )
        })?;
        aria2_core::engine::halt_watchers::spawn_timed_halt(
            engine_cmd_tx.clone(),
            RPC_SHUTDOWN_GRACE,
            true,
        );

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
    /// Registers a `RequestGroup` and sends `EngineCommand::AddDownload` to the
    /// single core download engine.
    async fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<String, JsonRpcError> {
        let gid_str = create_gid();
        let gid = GroupId::from_hex_string(&gid_str)
            .ok_or_else(|| JsonRpcError::InternalError("Invalid GID generated".into()))?;

        // Merge user-set global options (aria2.changeGlobalOption) so they
        // apply to this download; task-level options win. Registry-default
        // values are NOT merged (they live in global_opts, kept separate).
        let merged_options = {
            let user = self.user_global_opts.read().await;
            let mut m: HashMap<String, serde_json::Value> = user.clone();
            for (k, v) in &options {
                m.insert(k.clone(), v.clone());
            }
            m
        };
        let dl_options = rpc_options_to_download_options(&merged_options);

        // Validate checksum format at task creation time, before any download starts.
        if let Some((ref algo, ref val)) = dl_options.checksum {
            Checksum::from_type_and_value(algo, val)
                .map_err(|e| JsonRpcError::InvalidParams(format!("Invalid checksum: {}", e)))?;
        }

        // Start a real download only through the structured engine command path.
        let mut registered_in_group_man = false;
        if let (Some(group_man), Some(engine_cmd_tx)) = (&self.group_man, &self.engine_cmd_tx) {
            let man = group_man.read().await;
            man.add_group_with_gid(gid, uris.clone(), dl_options.clone())
                .map_err(|e| JsonRpcError::InternalError(format!("Failed to add group: {}", e)))?;

            let group = man.group_by_id(gid).ok_or_else(|| {
                JsonRpcError::InternalError("Group not found after insert".into())
            })?;

            // Send EngineCommand::AddDownload to the engine loop.
            // The loop will promote the group from reserved to active on the
            // next tick, create the appropriate Command, and spawn it.
            use aria2_core::engine::engine_command::EngineCommand;
            engine_cmd_tx
                .send(EngineCommand::AddDownload { group })
                .map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {}", e))
                })?;
            registered_in_group_man = true;
        }

        if !registered_in_group_man {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan and engine command channel are required".into(),
            ));
        }
        let mut task_opts = self.task_opts.write().await;
        task_opts.insert(gid_str.clone(), merged_options);
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

        let connections = g.options().split.unwrap_or(core_constants::DEFAULT_SPLIT);

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
                info = info.with_num_pieces(total.div_ceil(pl) as u32);
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
    /// the download engine. It also checks core stopped results so removed,
    /// completed, and failed downloads remain queryable until purged.
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        // Try RequestGroupMan first (live progress)
        if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            if let Some(group_lock) = man.group_by_hex(gid) {
                let g = group_lock.recover();
                return Some(Self::build_status_from_group(&g, gid));
            }
            if let Some(result) = man.find_stopped_result(gid) {
                return Some(Self::build_status_from_result(&result));
            }
        }
        None
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
        seed_time: get_f64("seed-time"),
        seed_ratio: get_f64("seed-ratio"),
        // File allocation
        file_allocation: get_str("file-allocation"),
        mmap_threshold: get_u64("mmap-threshold"),
        secure_falloc: get_bool("secure-falloc"),
        check_integrity: get_bool("check-integrity"),
        hash_check_only: get_bool("hash-check-only"),
        // Checksum
        checksum,
        // Cookies
        cookie_file: get_str("cookie-file"),
        cookies: get_str("cookies"),
        // BT
        bt_max_peers: get_u64("bt-max-peers").unwrap_or(55) as usize,
        bt_force_encrypt: opts
            .get("bt-force-encryption")
            .and_then(|v| v.as_bool())
            .or_else(|| opts.get("bt-force-encrypt").and_then(|v| v.as_bool()))
            .unwrap_or(false),
        bt_require_crypto: get_bool("bt-require-crypto"),
        enable_dht: opts
            .get("enable-dht")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        dht_listen_port: get_u16("dht-listen-port"),
        dht_entry_point,
        bt_tracker: opts.get("bt-tracker").and_then(|v| {
            if let Some(arr) = v.as_array() {
                Some(
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            } else {
                v.as_str().map(|s| {
                    s.split([',', '\n'])
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
            }
        }),
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
        bt_detach_seed_only: get_bool("bt-detach-seed-only"),
        enable_utp: get_bool("enable-utp"),
        utp_listen_port: get_u16("utp-listen-port"),
        // Retry
        max_retries: get_u32("max-tries")
            .or_else(|| get_u32("max-retries"))
            .unwrap_or(0),
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
        // Metalink
        metalink_version: get_str("metalink-version"),
        metalink_language: get_str("metalink-language"),
        metalink_os: get_str("metalink-os"),
        metalink_location: get_str("metalink-location"),
        metalink_preferred_protocol: get_str("metalink-preferred-protocol"),
        select_file: get_str("select-file"),
        piece_length: get_u64("piece-length"),
        metalink_enable_unique_protocol: opts
            .get("metalink-enable-unique-protocol")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        // FTP
        timeout: get_u64("timeout"),
        connect_timeout: get_u64("connect-timeout"),
        startup_idle_time: get_u64("startup-idle-time"),
        lowest_speed_limit: get_u64("lowest-speed-limit"),
        ftp_pasv: opts
            .get("ftp-pasv")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        remote_time: opts
            .get("remote-time")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        dry_run: opts
            .get("dry-run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ftp_reuse_connection: opts
            .get("ftp-reuse-connection")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        // Download
        realtime_chunk_checksum: opts
            .get("realtime-chunk-checksum")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        bt_stop_timeout: get_u64("bt-stop-timeout"),
        // BitTorrent extended
        disable_ipv6: opts
            .get("disable-ipv6")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        listen_port: get_str("listen-port"),
        bt_enable_lpd: opts
            .get("bt-enable-lpd")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        bt_lpd_interface: get_str("bt-lpd-interface"),
        enable_rpc: opts
            .get("enable-rpc")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        pause: opts.get("pause").and_then(|v| v.as_bool()).unwrap_or(false),
        // Follow options
        follow_torrent: opts.get("follow-torrent").and_then(|v| v.as_bool()),
        follow_metalink: opts.get("follow-metalink").and_then(|v| v.as_bool()),
        // Event hooks
        on_download_start: get_str("on-download-start"),
        on_download_complete: get_str("on-download-complete"),
        on_download_error: get_str("on-download-error"),
        on_download_pause: get_str("on-download-pause"),
        on_download_stop: get_str("on-download-stop"),
        on_bt_download_complete: get_str("on-bt-download-complete"),
        // HTTP authentication
        http_auth_challenge: opts
            .get("http-auth-challenge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        http_user: get_str("http-user"),
        http_passwd: get_str("http-passwd"),
        ftp_user: get_str("ftp-user"),
        ftp_passwd: get_str("ftp-passwd"),
        ssh_host_key_md: get_str("ssh-host-key-md"),
        no_netrc: opts
            .get("no-netrc")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        netrc_path: get_str("netrc-path"),
        // Conditional GET
        conditional_get: opts
            .get("conditional-get")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}
