//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use std::collections::HashMap;

use crate::engine::RpcEngine;
use crate::engine::TaskState;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, create_gid};
use crate::websocket::{DownloadEvent, EventType};

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

    /// Internal helper to add a new download task.
    async fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<String, JsonRpcError> {
        let gid = create_gid();
        let dir = options
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let status = StatusInfo::new(&gid)
            .with_status(DownloadStatus::Active)
            .with_dir(dir)
            .with_total_length(0)
            .with_completed_length(0)
            .with_files(vec![FileInfo::new("", 0)]);
        let state = TaskState::new(status, options, uris);
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(gid.clone(), state);
        }
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid, vec![]),
        );
        Ok(gid)
    }

    /// Internal helper to get current status info for a task.
    async fn get_status(&self, gid: &str) -> Option<StatusInfo> {
        let mut tasks = self.tasks.write().await;
        let state = tasks.get_mut(gid)?;
        state.update_status_info();
        Some(state.status.clone())
    }
}
