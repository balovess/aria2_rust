//! Task management RPC handlers.
//!
//! Handlers for creating, removing, pausing, and resuming download tasks.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, FileInfo, StatusInfo, UriEntry, UriStatus, create_gid};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::checksum::checksum::Checksum;
use aria2_core::config::project_initial_options;
use aria2_core::constants as core_constants;
use aria2_core::engine::command::Command;
use aria2_core::engine::engine_command::EngineCommand;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
#[cfg(feature = "metalink")]
use aria2_core::request::request_group::RequestGroup;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::ChangePositionMode;
use aria2_core::session::save_session_command::SaveSessionCommand;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::collections::HashMap;
#[cfg(feature = "metalink")]
use std::sync::Arc;

/// Delay between answering a shutdown RPC and actually halting the engine.
///
/// Mirrors the hard-coded `3_s` in C++ `RpcMethodImpl::goingShutdown()`, which
/// schedules a `TimedHaltCommand` so the JSON-RPC response is flushed before
/// the engine (and with it the RPC listener) goes away.
const RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

fn optional_position(req: &JsonRpcRequest, index: usize) -> Result<Option<usize>, JsonRpcError> {
    let Some(position) = req.get_optional_param::<i64>(index)? else {
        return Ok(None);
    };
    if position < 0 {
        return Err(JsonRpcError::RpcExecution(
            "Position must be greater than or equal to 0.".into(),
        ));
    }
    usize::try_from(position)
        .map(Some)
        .map_err(|_| JsonRpcError::RpcExecution("Position is out of range.".into()))
}

fn optional_position_owned(
    req: &mut JsonRpcRequest,
    index: usize,
) -> Result<Option<usize>, JsonRpcError> {
    let Some(position) = req.take_optional_param::<i64>(index)? else {
        return Ok(None);
    };
    if position < 0 {
        return Err(JsonRpcError::RpcExecution(
            "Position must be greater than or equal to 0.".into(),
        ));
    }
    usize::try_from(position)
        .map(Some)
        .map_err(|_| JsonRpcError::RpcExecution("Position is out of range.".into()))
}

fn decode_rpc_payload_sync(input: &str) -> Result<Vec<u8>, String> {
    let encoded = if input.starts_with("data:") {
        input.split(',').nth(1).unwrap_or("")
    } else {
        input
    };
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|error| format!("base64 decode failed: {error}"))
}

async fn decode_rpc_payload(input: String) -> Result<Vec<u8>, JsonRpcError> {
    tokio::task::spawn_blocking(move || decode_rpc_payload_sync(&input))
        .await
        .map_err(|error| JsonRpcError::InternalError(format!("base64 task failed: {error}")))?
        .map_err(JsonRpcError::InvalidParams)
}

pub(crate) struct PreparedTorrent {
    torrent_uri: String,
    additional_uris: Vec<String>,
    opts: HashMap<String, serde_json::Value>,
    position: Option<usize>,
}

#[cfg(feature = "metalink")]
pub(crate) struct PreparedMetalink {
    options: HashMap<String, serde_json::Value>,
    position: Option<usize>,
    resource_groups: Option<Vec<Arc<std::sync::RwLock<RequestGroup>>>>,
    #[cfg(feature = "bittorrent")]
    torrent_graphs: Option<Vec<MetalinkRequestGraph>>,
}

fn torrent_uri_from_decoded(decoded: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let prefix = &decoded[..decoded.len().min(32)];
    let mut uri = String::with_capacity("torrent://".len() + prefix.len() * 2);
    uri.push_str("torrent://");
    for byte in prefix {
        uri.push(HEX[(byte >> 4) as usize] as char);
        uri.push(HEX[(byte & 0x0f) as usize] as char);
    }
    uri
}

async fn prepare_torrent(
    torrent_data: String,
    additional_uris: Vec<String>,
    opts: HashMap<String, serde_json::Value>,
    position: Option<usize>,
) -> Result<PreparedTorrent, JsonRpcError> {
    let decoded_bytes = decode_rpc_payload(torrent_data).await?;
    if decoded_bytes.len() < 3
        || decoded_bytes[0] != b'd'
        || decoded_bytes[1] != b'8'
        || decoded_bytes[2] != b':'
    {
        return Err(JsonRpcError::InvalidParams(
            "Invalid BEncode data (not a .torrent file)".into(),
        ));
    }

    Ok(PreparedTorrent {
        torrent_uri: torrent_uri_from_decoded(&decoded_bytes),
        additional_uris,
        opts,
        position,
    })
}

fn validate_metalink_preview(decoded: &[u8]) -> Result<(), JsonRpcError> {
    let preview = String::from_utf8_lossy(&decoded[..decoded.len().min(200)]);
    if !preview.to_lowercase().contains("<metalink")
        && !preview.contains("urn:ietf:params:xml:ns:metalink")
    {
        return Err(JsonRpcError::InvalidParams(
            "Invalid Metalink XML data".into(),
        ));
    }
    Ok(())
}

impl RpcEngine {
    /// Handle `aria2.addUri` - Add a new download task from URI(s).
    pub async fn handle_add_uri(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let uris: Vec<String> = req.get_param(0)?;
        let opts: HashMap<String, serde_json::Value> =
            req.get_optional_param(1)?.unwrap_or_default();
        let position = optional_position(req, 2)?;
        self.handle_add_uri_values(req.id.clone(), uris, opts, position)
            .await
    }

    /// Owned network path for `aria2.addUri`. The request parser has already
    /// transferred ownership of the parameter DOM, so the large URI/options
    /// values can be deserialized without cloning them first.
    pub(crate) async fn handle_add_uri_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let uris: Vec<String> = req.take_param(0)?;
        let opts: HashMap<String, serde_json::Value> =
            req.take_optional_param(1)?.unwrap_or_default();
        let position = optional_position_owned(req, 2)?;
        self.handle_add_uri_values(req.id.clone(), uris, opts, position)
            .await
    }

    async fn handle_add_uri_values(
        &self,
        request_id: Option<serde_json::Value>,
        uris: Vec<String>,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid = self.add_task(uris, opts).await?;
        if let Some(pos) = position {
            let pos_req = JsonRpcRequest::new(
                "aria2.changePosition",
                serde_json::json!([&gid, pos as i64, "POS_SET"]),
            );
            let _ = self.handle_change_position(&pos_req).await;
        }
        Ok(JsonRpcResponse::success(
            request_id.unwrap_or_default(),
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
    pub async fn handle_add_torrent(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let torrent_data: String = req.get_param(0)?;

        // Match the original positional signature exactly: [torrent, uris?,
        // opts?, pos?]. A present value is validated at its documented slot.
        let additional_uris: Vec<String> = req.get_optional_param(1)?.unwrap_or_default();
        let opts: HashMap<String, serde_json::Value> =
            req.get_optional_param(2)?.unwrap_or_default();
        let position = optional_position(req, 3)?;
        self.handle_add_torrent_values(
            req.id.clone(),
            torrent_data,
            additional_uris,
            opts,
            position,
        )
        .await
    }

    /// Owned network path for `aria2.addTorrent`.
    #[cfg(feature = "bittorrent")]
    pub(crate) async fn handle_add_torrent_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let prepared = self.prepare_add_torrent_owned(req).await?;
        self.commit_prepared_torrent(req.id.clone(), prepared).await
    }

    /// Decode and validate the owned torrent request before waiting for the
    /// lifecycle gate. Only the short synthetic torrent URI survives into the
    /// commit phase; the decoded payload is dropped immediately.
    #[cfg(feature = "bittorrent")]
    pub(crate) async fn prepare_add_torrent_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<PreparedTorrent, JsonRpcError> {
        let torrent_data: String = req.take_param(0)?;
        let additional_uris: Vec<String> = req.take_optional_param(1)?.unwrap_or_default();
        let opts: HashMap<String, serde_json::Value> =
            req.take_optional_param(2)?.unwrap_or_default();
        let position = optional_position_owned(req, 3)?;
        prepare_torrent(torrent_data, additional_uris, opts, position).await
    }

    /// Commit a torrent after its CPU-heavy input preparation has completed.
    pub(crate) async fn commit_prepared_torrent(
        &self,
        request_id: Option<serde_json::Value>,
        prepared: PreparedTorrent,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let mut uris = Vec::with_capacity(1 + prepared.additional_uris.len());
        uris.push(prepared.torrent_uri);
        uris.extend(prepared.additional_uris);
        let gid = self.add_task(uris, prepared.opts).await?;
        if let Some(pos) = prepared.position {
            let pos_req = JsonRpcRequest::new(
                "aria2.changePosition",
                serde_json::json!([&gid, pos as i64, "POS_SET"]),
            );
            let _ = self.handle_change_position(&pos_req).await;
        }
        Ok(JsonRpcResponse::success(
            request_id.unwrap_or_default(),
            gid,
        ))
    }

    async fn handle_add_torrent_values(
        &self,
        request_id: Option<serde_json::Value>,
        torrent_data: String,
        additional_uris: Vec<String>,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let prepared = prepare_torrent(torrent_data, additional_uris, opts, position).await?;
        self.commit_prepared_torrent(request_id, prepared).await
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
        let opts: HashMap<String, serde_json::Value> =
            req.get_optional_param(1)?.unwrap_or_default();
        let position = optional_position(req, 2)?;
        self.handle_add_metalink_values(req.id.clone(), metalink_data, opts, position)
            .await
    }

    /// Owned network path for `aria2.addMetalink`.
    #[cfg(feature = "metalink")]
    pub(crate) async fn handle_add_metalink_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let metalink_data: String = req.take_param(0)?;
        let opts: HashMap<String, serde_json::Value> =
            req.take_optional_param(1)?.unwrap_or_default();
        let position = optional_position_owned(req, 2)?;
        let prepared = self
            .prepare_add_metalink(metalink_data, opts, position)
            .await?;
        self.commit_prepared_metalink(req.id.clone(), prepared)
            .await
    }

    #[cfg(feature = "metalink")]
    pub(crate) async fn prepare_add_metalink_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<PreparedMetalink, JsonRpcError> {
        let metalink_data: String = req.take_param(0)?;
        let opts: HashMap<String, serde_json::Value> =
            req.take_optional_param(1)?.unwrap_or_default();
        let position = optional_position_owned(req, 2)?;
        self.prepare_add_metalink(metalink_data, opts, position)
            .await
    }

    #[cfg(feature = "metalink")]
    async fn prepare_add_metalink(
        &self,
        metalink_data: String,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<PreparedMetalink, JsonRpcError> {
        let decoded_bytes = decode_rpc_payload(metalink_data).await?;
        validate_metalink_preview(&decoded_bytes)?;
        self.prepare_add_metalink_from_decoded(decoded_bytes, opts, position)
            .await
    }

    #[cfg(feature = "metalink")]
    pub(crate) async fn commit_prepared_metalink(
        &self,
        request_id: Option<serde_json::Value>,
        prepared: PreparedMetalink,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let mut gids = Vec::new();
        if let Some(group_man) = &self.group_man {
            if let Some(resource_groups) = prepared.resource_groups {
                for group in resource_groups {
                    let gid = group.recover().gid();
                    group_man.add_group_arc(group);
                    gids.push(gid.to_hex_string());
                }
            }

            #[cfg(feature = "bittorrent")]
            if let Some(torrent_graphs) = prepared.torrent_graphs {
                for graph in torrent_graphs {
                    let (_, payload_gid) = group_man
                        .add_metalink_graph(graph)
                        .map_err(|error| JsonRpcError::InternalError(error.to_string()))?;
                    gids.push(payload_gid.to_hex_string());
                }
            }

            if !gids.is_empty() {
                if let Some(pos) = prepared.position {
                    let first_gid = GroupId::from_hex_string(&gids[0]).ok_or_else(|| {
                        JsonRpcError::InternalError("Invalid Metalink GID generated".into())
                    })?;
                    let position = i32::try_from(pos).map_err(|_| {
                        JsonRpcError::InvalidParams("position is out of range".into())
                    })?;
                    group_man
                        .change_position(first_gid, position, ChangePositionMode::SetFromStart)
                        .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
                }
                return Ok(JsonRpcResponse::success(
                    request_id.unwrap_or_default(),
                    gids,
                ));
            }
        }

        self.commit_metalink_fallback(request_id, prepared.options, prepared.position)
            .await
    }

    async fn handle_add_metalink_values(
        &self,
        request_id: Option<serde_json::Value>,
        metalink_data: String,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let decoded_bytes = decode_rpc_payload(metalink_data).await?;

        validate_metalink_preview(&decoded_bytes)?;

        #[cfg(feature = "metalink")]
        {
            let prepared = self
                .prepare_add_metalink_from_decoded(decoded_bytes, opts, position)
                .await?;
            return self.commit_prepared_metalink(request_id, prepared).await;
        }

        #[cfg(not(feature = "metalink"))]
        self.commit_metalink_fallback(request_id, opts, position)
            .await
    }

    #[cfg(feature = "metalink")]
    async fn prepare_add_metalink_from_decoded(
        &self,
        decoded_bytes: Vec<u8>,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<PreparedMetalink, JsonRpcError> {
        let Some(group_man) = &self.group_man else {
            return Ok(PreparedMetalink {
                options: opts,
                position,
                resource_groups: None,
                #[cfg(feature = "bittorrent")]
                torrent_graphs: None,
            });
        };

        let options = rpc_options_to_download_options(&opts)?;
        let data = Arc::new(decoded_bytes);
        let resource_data = Arc::clone(&data);
        let resource_man = Arc::clone(group_man);
        let resource_options = options.clone();
        let resource_groups = tokio::task::spawn_blocking(move || {
            let converter =
                aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup::new();
            let mut gid_source = std::iter::from_fn(|| Some(resource_man.next_available_gid()));
            converter
                .create_resource_groups_from_bytes(
                    resource_data.as_ref(),
                    &resource_options,
                    &mut gid_source,
                )
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| JsonRpcError::InternalError(format!("Metalink worker failed: {error}")))?
        .map_err(JsonRpcError::InvalidParams)?;

        #[cfg(feature = "bittorrent")]
        let torrent_graphs = {
            let graph_data = Arc::clone(&data);
            let graph_man = Arc::clone(group_man);
            let graph_options = options;
            tokio::task::spawn_blocking(move || {
                let converter =
                    aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup::new();
                let mut gid_source = std::iter::from_fn(|| Some(graph_man.next_available_gid()));
                converter
                    .create_torrent_graphs_from_bytes(
                        graph_data.as_ref(),
                        &graph_options,
                        &mut gid_source,
                    )
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| {
                JsonRpcError::InternalError(format!("Metalink worker failed: {error}"))
            })?
            .map_err(JsonRpcError::InvalidParams)?
        };

        Ok(PreparedMetalink {
            options: opts,
            position,
            resource_groups: Some(resource_groups),
            #[cfg(feature = "bittorrent")]
            torrent_graphs: Some(torrent_graphs),
        })
    }

    async fn commit_metalink_fallback(
        &self,
        request_id: Option<serde_json::Value>,
        opts: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
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
        Ok(JsonRpcResponse::success(
            request_id.unwrap_or_default(),
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
        let enqueue_command = if let Some(group_man) = &self.group_man {
            let man = group_man;
            man.remove_group(gid_parsed)
                .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
            // Reserved groups are removed synchronously and are already in the
            // stopped-result store. Active groups remain indexed while their
            // command drains and therefore still need an engine notification.
            man.group_by_hex(&gid).is_some()
        } else {
            true
        };
        if enqueue_command {
            engine_cmd_tx
                .send(EngineCommand::RemoveDownload { gid: gid_parsed })
                .map_err(|e| {
                    JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                })?;
        }
        let _ = self
            .event_publisher
            .publish(EventType::DownloadStop, DownloadEvent::download_stop(&gid));
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
        if let Some(group_man) = &self.group_man {
            group_man
                .pause_group(gid_parsed)
                .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
        }
        engine_cmd_tx
            .send(EngineCommand::Pause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        let _ = self.event_publisher.publish(
            EventType::DownloadPause,
            DownloadEvent::download_pause(&gid),
        );
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
        if let Some(group_man) = &self.group_man {
            group_man
                .force_pause_group(gid_parsed)
                .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
        }
        engine_cmd_tx
            .send(EngineCommand::ForcePause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        let _ = self.event_publisher.publish(
            EventType::DownloadPause,
            DownloadEvent::download_pause(&gid),
        );
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!(gid),
        ))
    }

    /// Handle `aria2.unpause` - Resume a paused task.
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
        // Commit the externally visible transition before returning the RPC
        // response. The original aria2 contract changes a paused group to
        // WAITING synchronously, then requests a queue check. The engine
        // command below performs that scheduling pass and is idempotent when
        // it observes the already-resumed group.
        if let Some(group_man) = &self.group_man {
            group_man
                .unpause_group(gid_parsed)
                .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
        }
        engine_cmd_tx
            .send(EngineCommand::Unpause { gid: gid_parsed })
            .map_err(|e| {
                JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
            })?;
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid),
        );
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
        let keys = crate::handlers::status::status_keys_for_request(req, 1)?;
        let key_filter = crate::handlers::status::status_key_filter(&keys);
        match self.get_status(&gid).await {
            Some(status) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                crate::handlers::status::status_to_json_with_filter(status, key_filter.as_ref())?,
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
        let parsed_gids = gids
            .iter()
            .map(|gid| {
                GroupId::from_hex_string(gid)
                    .ok_or_else(|| JsonRpcError::InvalidParams("Invalid GID".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let enqueue_commands = if let Some(group_man) = &self.group_man {
            let man = group_man;
            for gid in &gids {
                if man.group_by_hex(gid).is_none() {
                    return Err(JsonRpcError::RpcExecution(format!("GID {gid} not found")));
                }
            }
            let mut enqueue_commands = Vec::with_capacity(parsed_gids.len());
            for gid in &parsed_gids {
                man.force_remove_group(*gid)
                    .map_err(|error| JsonRpcError::RpcExecution(error.to_string()))?;
                // Reserved groups are complete from the RPC caller's point of
                // view. Active groups remain indexed until the command exits.
                enqueue_commands.push(man.find_group(*gid).is_some());
            }
            enqueue_commands
        } else {
            vec![true; parsed_gids.len()]
        };
        for ((gid, gid_parsed), enqueue_command) in
            gids.iter().zip(parsed_gids).zip(enqueue_commands)
        {
            if enqueue_command {
                engine_cmd_tx
                    .send(EngineCommand::ForceRemoveDownload { gid: gid_parsed })
                    .map_err(|e| {
                        JsonRpcError::InternalError(format!("Failed to send engine command: {e}"))
                    })?;
            }
            let _ = self
                .event_publisher
                .publish(EventType::DownloadStop, DownloadEvent::download_stop(gid));
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
        let file_index: i64 = req.get_param(1)?;
        if file_index < 1 {
            return Err(JsonRpcError::InvalidParams(
                "fileIndex must be at least 1".into(),
            ));
        }
        let del_uris: Vec<String> = req.get_param(2)?;
        let add_uris: Vec<String> = req.get_param(3)?;
        let position = optional_position(req, 4)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let man = group_man;
        let group = man
            .group_by_hex(&gid)
            .ok_or_else(|| JsonRpcError::RpcExecution(format!("GID {} not found", gid)))?;
        let result = group
            .write()
            .map_err(|_| JsonRpcError::InternalError("Failed to lock request group".into()))?
            .change_uris(file_index as usize, &del_uris, &add_uris, position)
            .map_err(|e| JsonRpcError::RpcExecution(e.to_string()))?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::json!([result.0, result.1]),
        ))
    }

    /// Owned network path for `aria2.changeUri`. The delete/add URI arrays
    /// can be large, so consume them from the already-owned request.
    pub(crate) async fn handle_change_uri_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.take_param(0)?;
        let file_index: i64 = req.take_param(1)?;
        if file_index < 1 {
            return Err(JsonRpcError::InvalidParams(
                "fileIndex must be at least 1".into(),
            ));
        }
        let del_uris: Vec<String> = req.take_param(2)?;
        let add_uris: Vec<String> = req.take_param(3)?;
        let position = optional_position_owned(req, 4)?;
        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let group = group_man
            .group_by_hex(&gid)
            .ok_or_else(|| JsonRpcError::RpcExecution(format!("GID {} not found", gid)))?;
        let result = group
            .write()
            .map_err(|_| JsonRpcError::InternalError("Failed to lock request group".into()))?
            .change_uris(file_index as usize, &del_uris, &add_uris, position)
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
    /// The request's positional parameters are ignored. The engine's
    /// configured `--save-session` path is always used.
    /// - The real `RequestGroup` state is serialized through the core manager.
    pub async fn handle_save_session(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // C++ ignores request parameters and always reads PREF_SAVE_SESSION.
        // Keep that seam exact so a client cannot redirect the server's
        // session output by sending an extension-only argument.
        let path = self.save_session_path.clone().ok_or_else(|| {
            JsonRpcError::RpcExecution("Filename is not given. Set --save-session.".into())
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
        let man = group_man;
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
        // Shutdown covers both active and waiting work. A request can reach
        // RPC before the engine's next promotion tick, and that queued task
        // must still be included in the shutdown acknowledgement.
        let active_count = group_man.count();
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
        let cancelled_count = {
            let man = group_man;
            let count = man.count();
            // Queued groups have no command handle to drain, so remove them
            // synchronously. Active groups remain in the manager until the
            // delayed force-halt command reaches the engine loop.
            man.force_remove_reserved();
            count
        };
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
    /// Registers a `RequestGroup` in the shared reserved queue and notifies
    /// the engine loop with one idempotent `AddDownload` command.
    ///
    /// Registration happens before the command is sent so RPC status queries
    /// can observe the task immediately. The engine-side insert is guarded by
    /// the GID check in `add_group_arc`, so this notification cannot enqueue a
    /// second copy of the group.
    async fn add_task(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
    ) -> Result<String, JsonRpcError> {
        let gid_str = create_gid();
        let gid = GroupId::from_hex_string(&gid_str)
            .ok_or_else(|| JsonRpcError::InternalError("Invalid GID generated".into()))?;

        // A new request group inherits the canonical global option snapshot,
        // including startup CLI/config and runtime global changes. Per-task
        // options take precedence.
        let mut merged_options = self.global_opts.read().await.clone();
        merged_options.extend(options);
        let dl_options = rpc_options_to_download_options(&merged_options)?;
        let option_snapshot = project_initial_options(merged_options);

        // Validate checksum format at task creation time, before any download starts.
        if let Some((ref algo, ref val)) = dl_options.checksum {
            Checksum::from_type_and_value(algo, val)
                .map_err(|e| JsonRpcError::InvalidParams(format!("Invalid checksum: {}", e)))?;
        }

        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::InternalError("RequestGroupMan is required".into()))?;
        let engine_cmd_tx = self
            .engine_cmd_tx
            .as_ref()
            .ok_or_else(|| {
                JsonRpcError::InternalError("Engine command channel is required".into())
            })?
            .clone();

        let group = {
            // Serialize the check-and-insert so concurrent RPC calls cannot
            // observe the same GID as available at the same time.
            let man = group_man;
            man.add_group_with_gid(gid, uris, dl_options)
                .map_err(|e| JsonRpcError::InternalError(format!("Failed to add group: {}", e)))?;
            let group = man.group_by_id(gid).ok_or_else(|| {
                JsonRpcError::InternalError("Group not found after insert".into())
            })?;
            group
                .recover_mut()
                .set_option_snapshot(option_snapshot.clone());
            group
        };

        if let Err(error) = engine_cmd_tx.send(EngineCommand::AddDownload { group }) {
            // The group has not been promoted yet, so it can be removed
            // synchronously if the engine has already gone away.
            let _ = group_man.remove_group_by_id(gid);
            return Err(JsonRpcError::InternalError(format!(
                "Failed to send engine command: {error}"
            )));
        }

        // C++ aria2 notification only includes gid (no files field)
        let _ = self.event_publisher.publish(
            EventType::DownloadStart,
            DownloadEvent::download_start(&gid_str),
        );
        Ok(gid_str)
    }

    /// Build the file list exposed by `getFiles` and `tellStatus.files`.
    ///
    /// The original implementation returns every file entry, including
    /// entries excluded by `select-file`; selection is represented by the
    /// `selected` field. Keeping this conversion in one helper prevents the
    /// two RPC methods from drifting apart.
    pub(crate) fn build_file_infos(
        g: &aria2_core::request::request_group::RequestGroup,
        completed: u64,
    ) -> Vec<FileInfo> {
        let fallback_path = || {
            let options = g.options();
            let name = options
                .out
                .clone()
                .or_else(|| {
                    g.uris().first().and_then(|uri| {
                        url::Url::parse(uri)
                            .ok()
                            .and_then(|parsed| {
                                parsed.path_segments()?.next_back().map(str::to_owned)
                            })
                            .filter(|segment| !segment.is_empty())
                    })
                })
                .unwrap_or_default();
            if name.is_empty() {
                return name;
            }
            match options.dir.as_deref().filter(|dir| !dir.is_empty()) {
                Some(dir) => std::path::PathBuf::from(dir)
                    .join(name)
                    .to_string_lossy()
                    .into_owned(),
                None => name,
            }
        };

        if let Some(context) = g.get_download_context() {
            return context
                .get_file_entries()
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    let mut info = FileInfo::new(
                        if file.path().is_empty() {
                            fallback_path()
                        } else {
                            file.path().to_owned()
                        },
                        file.length(),
                    )
                    .with_index(index + 1)
                    .with_completed(completed.saturating_sub(file.offset()).min(file.length()))
                    .with_uris(Self::build_uri_entries(file));
                    info.selected = file.is_requested();
                    info
                })
                .collect();
        }

        let mut info = FileInfo::new(fallback_path(), g.get_total_length_atomic())
            .with_index(1)
            .with_completed(completed)
            .with_uris(g.uris().iter().cloned().map(UriEntry::new).collect());
        info.selected = true;
        vec![info]
    }

    /// Convert the core URI lifecycle into aria2's public URI vocabulary.
    ///
    /// The core keeps `spent` as a useful lifecycle state. The C++ RPC
    /// adapter exposes dispatched URIs as `used`, so that internal state must
    /// never leak through the external seam.
    pub(crate) fn build_uri_entries(
        file: &aria2_core::download::file_entry::FileEntry,
    ) -> Vec<UriEntry> {
        let remaining = file.remaining_uris();
        file.uris()
            .into_iter()
            .map(|uri| UriEntry {
                status: if remaining.iter().any(|candidate| candidate == &uri) {
                    UriStatus::Waiting
                } else {
                    UriStatus::Used
                },
                uri,
            })
            .collect()
    }

    /// Convert a stopped core snapshot without dropping file selection or URI
    /// state. This is the same wire adapter used by `tellStatus.files` and
    /// `getFiles` for completed, removed, and failed downloads.
    pub(crate) fn build_file_infos_from_result(
        result: &aria2_core::request::request_group::download_result::DownloadResult,
    ) -> Vec<FileInfo> {
        result
            .files
            .iter()
            .map(|file| {
                let mut info = FileInfo::new(file.path.clone(), file.length)
                    .with_index(file.index)
                    .with_completed(file.completed_length)
                    .with_uris(
                        file.uris
                            .iter()
                            .map(|uri| UriEntry {
                                uri: uri.uri.clone(),
                                status: UriStatus::from_core_status(&uri.status),
                            })
                            .collect(),
                    );
                info.selected = file.selected;
                info
            })
            .collect()
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

        let connections = g.options().split.unwrap_or(core_constants::DEFAULT_SPLIT);
        let files = Self::build_file_infos(g, completed);

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
            let man = group_man;
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

/// Convert RPC option values through the shared aria2 string parser.
///
/// The public RPC wire format uses strings for options. The core adapter also
/// accepts numeric/boolean JSON values for existing Rust callers and
/// canonicalizes them before parsing.
fn rpc_options_to_download_options(
    opts: &HashMap<String, serde_json::Value>,
) -> Result<DownloadOptions, JsonRpcError> {
    DownloadOptions::try_from_rpc_options(opts).map_err(JsonRpcError::RpcExecution)
}
