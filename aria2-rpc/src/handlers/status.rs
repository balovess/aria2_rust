//! Status query RPC handlers.
//!
//! Handlers for querying download status and global statistics.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, GlobalStat, StatusInfo};

/// Apply the optional `keys` filter to a serialized StatusInfo value.
///
/// Per the aria2 RPC spec, when the caller supplies a `keys` array, the
/// response must contain only the requested top-level fields. The `gid`
/// field is always present (required by clients to identify the row).
/// Unknown keys in the filter are silently ignored to match original aria2
/// behavior.
///
/// - If `keys` is `None` or empty, the value is returned unchanged.
/// - If `value` is not an object, it is returned unchanged.
pub(crate) fn apply_keys_filter(
    value: serde_json::Value,
    keys: Option<&[String]>,
) -> serde_json::Value {
    let Some(keys) = keys else {
        return value;
    };
    if keys.is_empty() {
        return value;
    }
    let Some(obj) = value.as_object() else {
        return value;
    };
    let mut filtered = serde_json::Map::with_capacity(keys.len() + 1);
    // gid is always present per aria2 protocol
    if let Some(gid) = obj.get("gid").cloned() {
        filtered.insert("gid".to_string(), gid);
    }
    for k in keys {
        if k == "gid" {
            continue;
        }
        if let Some(v) = obj.get(k).cloned() {
            filtered.insert(k.clone(), v);
        }
    }
    serde_json::Value::Object(filtered)
}

/// Parse the `keys` array parameter from a request at the given position.
///
/// Returns `None` if the parameter is absent or null. Returns `Some(Vec)` if
/// present (possibly empty). Errors on non-array types.
pub(crate) fn parse_keys_param(
    req: &JsonRpcRequest,
    pos: usize,
) -> Result<Option<Vec<String>>, JsonRpcError> {
    match req.get_param::<serde_json::Value>(pos) {
        Ok(v) if v.is_null() => Ok(None),
        Ok(v) => {
            let arr: Vec<String> = serde_json::from_value(v).map_err(|_| {
                JsonRpcError::InvalidParams(format!(
                    "keys (param[{}]) must be an array of strings",
                    pos
                ))
            })?;
            Ok(Some(arr))
        }
        Err(_) => Ok(None),
    }
}

impl RpcEngine {
    /// Handle `aria2.tellActive` - List all active/running downloads.
    ///
    /// Optional `keys` parameter (param[0]) filters which fields are returned
    /// per the aria2 RPC spec. `gid` is always included.
    pub async fn handle_tell_active(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Bridge completed groups before querying to ensure stopped tasks
        // are visible via tellStopped and removed from tellActive results.
        self.bridge_completed_groups().await;
        let keys = parse_keys_param(req, 0)?;
        let active: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                let status = g.status().await;
                if status.is_active() {
                    let gid_hex = gid.to_hex_string();
                    let total = g.get_total_length_atomic();
                    let completed = g.get_completed_length();
                    result.push(
                        StatusInfo::new(&gid_hex)
                            .with_status(status)
                            .with_total_length(total)
                            .with_completed_length(completed)
                            .with_download_speed(g.get_download_speed_cached())
                            .with_upload_length(g.get_uploaded_length())
                            .with_upload_speed(0)
                            .with_connections(
                                g.options()
                                    .split
                                    .unwrap_or(aria2_core::constants::DEFAULT_SPLIT)
                                    as u16,
                            ),
                    );
                }
            }
            result
        } else {
            let tasks = self.tasks.read().await;
            tasks
                .values()
                .filter(|s| s.status.status.is_active())
                .map(|s| s.status.clone())
                .collect()
        };
        let mut value = serde_json::to_value(&active)
            .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?;
        if keys.is_some() {
            if let Some(arr) = value.as_array_mut() {
                for item in arr.iter_mut() {
                    *item = apply_keys_filter(item.clone(), keys.as_deref());
                }
            }
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            value,
        ))
    }

    /// Handle `aria2.tellWaiting` - List waiting/queued downloads with pagination.
    ///
    /// Parameters: `offset` (param[0]), `num` (param[1]), optional `keys` (param[2]).
    pub async fn handle_tell_waiting(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Bridge completed groups so they are excluded from waiting list
        // and visible via tellStopped.
        self.bridge_completed_groups().await;
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let keys = parse_keys_param(req, 2)?;
        let waiting: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                if g.status().await == DownloadStatus::Waiting {
                    let gid_hex = gid.to_hex_string();
                    let total = g.get_total_length_atomic();
                    let completed = g.get_completed_length();
                    result.push(
                        StatusInfo::new(&gid_hex)
                            .with_status(DownloadStatus::Waiting)
                            .with_total_length(total)
                            .with_completed_length(completed)
                            .with_download_speed(0)
                            .with_upload_speed(0)
                            .with_connections(
                                g.options()
                                    .split
                                    .unwrap_or(aria2_core::constants::DEFAULT_SPLIT)
                                    as u16,
                            ),
                    );
                }
            }
            result.into_iter().skip(offset).take(num).collect()
        } else {
            let tasks = self.tasks.read().await;
            tasks
                .values()
                .filter(|s| s.status.status == DownloadStatus::Waiting)
                .skip(offset.min(tasks.len()))
                .take(num)
                .map(|s| s.status.clone())
                .collect()
        };
        let mut value = serde_json::to_value(&waiting)
            .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?;
        if keys.is_some() {
            if let Some(arr) = value.as_array_mut() {
                for item in arr.iter_mut() {
                    *item = apply_keys_filter(item.clone(), keys.as_deref());
                }
            }
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            value,
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
    ///
    /// Parameters: `offset` (param[0]), `num` (param[1]), optional `keys` (param[2]).
    pub async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        // Bridge completed groups into stopped_tasks so they appear
        // in tellStopped results.
        self.bridge_completed_groups().await;
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let keys = parse_keys_param(req, 2)?;
        let stopped = self.stopped_tasks.read().await;
        let result: Vec<&StatusInfo> = stopped
            .iter()
            .skip(offset.min(stopped.len()))
            .take(num)
            .collect();
        let mut value = serde_json::to_value(&result)
            .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?;
        if keys.is_some() {
            if let Some(arr) = value.as_array_mut() {
                for item in arr.iter_mut() {
                    *item = apply_keys_filter(item.clone(), keys.as_deref());
                }
            }
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            value,
        ))
    }

    /// Handle `aria2.getGlobalStat` - Get global download statistics.
    ///
    /// Aggregates live speeds and counts from `RequestGroupMan` when available.
    /// The request `id` is preserved on the response so callers (including
    /// WebSocket batch callers) can correlate the result.
    pub async fn handle_global_stat(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        // Bridge completed groups so the stopped count reflects actual
        // finished downloads (groups just completed are in `stopped_tasks`).
        self.bridge_completed_groups().await;
        let (dl_speed, ul_speed, active, waiting, stopped) =
            if let Some(group_man) = &self.group_man {
                let man = group_man.read().await;
                let mut dl = 0u64;
                let mut ul = 0u64;
                let mut active_n = 0usize;
                let mut waiting_n = 0usize;
                for (_, group_lock) in man.all_groups() {
                    let g = group_lock.read().await;
                    dl += g.get_download_speed_cached();
                    // Uploaded length accumulates across all groups.
                    ul += g.get_uploaded_length();
                    match g.status().await {
                        DownloadStatus::Active => active_n += 1,
                        DownloadStatus::Waiting | DownloadStatus::Paused => waiting_n += 1,
                        // After bridge_completed_groups, Completed/Error groups
                        // are no longer in RequestGroupMan.
                        _ => {}
                    }
                }
                let stopped_n = self.stopped_tasks.read().await.len();
                (dl, ul, active_n, waiting_n, stopped_n)
            } else {
                let tasks = self.tasks.read().await;
                let (a, w): (Vec<_>, Vec<_>) =
                    tasks.values().partition(|s| s.status.status.is_active());
                let stopped_n = self.stopped_tasks.read().await.len();
                (0, 0, a.len(), w.len(), stopped_n)
            };
        let stat = GlobalStat::from_numbers(
            dl_speed,
            ul_speed,
            active,
            waiting,
            stopped,
            stopped,
        );
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), stat.to_json_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::RpcEngine;
    use crate::json_rpc::JsonRpcRequest;
    use crate::types::{DownloadStatus, StatusInfo};

    fn make_status_with_fields(gid: &str) -> StatusInfo {
        StatusInfo::new(gid)
            .with_status(DownloadStatus::Active)
            .with_total_length(1000)
            .with_completed_length(500)
            .with_download_speed(100)
            .with_upload_speed(50)
            .with_connections(3)
            .with_dir("/tmp")
    }

    #[test]
    fn test_apply_keys_filter_none_returns_unchanged() {
        let status = make_status_with_fields("abc123");
        let value = serde_json::to_value(&status).unwrap();
        let filtered = apply_keys_filter(value.clone(), None);
        assert_eq!(filtered, value);
    }

    #[test]
    fn test_apply_keys_filter_empty_returns_unchanged() {
        let status = make_status_with_fields("abc123");
        let value = serde_json::to_value(&status).unwrap();
        let filtered = apply_keys_filter(value.clone(), Some(&[]));
        assert_eq!(filtered, value);
    }

    #[test]
    fn test_apply_keys_filter_subset_includes_gid_always() {
        let status = make_status_with_fields("abc123");
        let value = serde_json::to_value(&status).unwrap();
        let keys = vec!["totalLength".to_string(), "downloadSpeed".to_string()];
        let filtered = apply_keys_filter(value, Some(&keys));
        let obj = filtered.as_object().unwrap();
        // gid always present
        assert_eq!(obj.len(), 3);
        assert_eq!(obj["gid"], "abc123");
        assert_eq!(obj["totalLength"], "1000");
        assert_eq!(obj["downloadSpeed"], "100");
        // filtered out
        assert!(!obj.contains_key("completedLength"));
        assert!(!obj.contains_key("uploadSpeed"));
        assert!(!obj.contains_key("connections"));
        assert!(!obj.contains_key("dir"));
    }

    #[test]
    fn test_apply_keys_filter_unknown_keys_ignored() {
        let status = make_status_with_fields("abc123");
        let value = serde_json::to_value(&status).unwrap();
        let keys = vec!["nonExistentField".to_string(), "totalLength".to_string()];
        let filtered = apply_keys_filter(value, Some(&keys));
        let obj = filtered.as_object().unwrap();
        // only gid (always) + totalLength
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("gid"));
        assert!(obj.contains_key("totalLength"));
        assert!(!obj.contains_key("nonExistentField"));
    }

    #[test]
    fn test_apply_keys_filter_gid_only() {
        let status = make_status_with_fields("abc123");
        let value = serde_json::to_value(&status).unwrap();
        let keys = vec!["gid".to_string()];
        let filtered = apply_keys_filter(value, Some(&keys));
        let obj = filtered.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["gid"], "abc123");
    }

    #[test]
    fn test_apply_keys_filter_on_non_object_returns_unchanged() {
        let value = serde_json::json!(42);
        let keys = vec!["totalLength".to_string()];
        let filtered = apply_keys_filter(value.clone(), Some(&keys));
        assert_eq!(filtered, value);
    }

    #[tokio::test]
    async fn test_tell_status_keys_filter_only_returns_requested_fields() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // Request only totalLength and status (gid is always included)
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!([gid, ["totalLength", "status"]]),
        )
        .with_id(2);
        let resp = engine.handle_request(&tell_req).await;
        assert!(resp.is_success());
        let status = resp.result.unwrap();
        let obj = status.as_object().expect("should be object");
        // gid + totalLength + status
        assert_eq!(obj.len(), 3, "should only have 3 fields");
        assert!(obj.contains_key("gid"));
        assert!(obj.contains_key("totalLength"));
        assert!(obj.contains_key("status"));
        // filtered out
        assert!(!obj.contains_key("completedLength"));
        assert!(!obj.contains_key("downloadSpeed"));
    }

    #[tokio::test]
    async fn test_tell_status_keys_null_returns_all_fields() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // null keys = no filter
        let tell_req =
            JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid, null])).with_id(2);
        let resp = engine.handle_request(&tell_req).await;
        assert!(resp.is_success());
        let status = resp.result.unwrap();
        let obj = status.as_object().expect("should be object");
        // Should have many fields including completedLength
        assert!(obj.contains_key("gid"));
        assert!(obj.contains_key("status"));
        assert!(obj.len() > 3, "should return all fields when keys=null");
    }

    #[tokio::test]
    async fn test_tell_status_keys_invalid_type_returns_error() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        let add_resp = engine.handle_request(&add_req).await;
        let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

        // keys as object (not array) should error
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!([gid, {"not": "array"}]),
        )
        .with_id(2);
        let resp = engine.handle_request(&tell_req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32602); // InvalidParams
    }

    #[tokio::test]
    async fn test_tell_active_keys_filter() {
        let engine = RpcEngine::new();
        let add_req =
            JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
        engine.handle_request(&add_req).await;

        // tellActive with keys filter
        let req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([["gid", "status"]]))
            .with_id(2);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success());
        let arr = resp.result.unwrap().as_array().unwrap().clone();
        assert!(!arr.is_empty());
        for item in &arr {
            let obj = item.as_object().unwrap();
            // Should only contain gid + status (gid is always present,
            // status is requested)
            assert!(obj.contains_key("gid"));
            assert!(obj.contains_key("status"));
            // Should NOT contain other fields
            assert!(
                !obj.contains_key("totalLength"),
                "totalLength should be filtered out"
            );
        }
    }
}
