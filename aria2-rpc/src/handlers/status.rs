//! Status query RPC handlers.
//!
//! Handlers for querying download status and global statistics.

use std::sync::atomic::Ordering;

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, GlobalStat, StatusInfo};

impl RpcEngine {
    /// Handle `aria2.tellActive` - List all active/running downloads.
    ///
    /// When `RequestGroupMan` is available, iterates all registered groups and
    /// reads live progress from their atomic fields. Otherwise falls back to
    /// the placeholder `tasks` map.
    pub async fn handle_tell_active(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let active: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                if g.status().await.is_active() {
                    let gid_hex = gid.to_hex_string();
                    result.push(Self::build_status_from_group(&g, &gid_hex).await);
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
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(active)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.tellWaiting` - List waiting/queued downloads with pagination.
    pub async fn handle_tell_waiting(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let waiting: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                if g.status().await == DownloadStatus::Waiting {
                    let gid_hex = gid.to_hex_string();
                    result.push(Self::build_status_from_group(&g, &gid_hex).await);
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
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(waiting)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
    ///
    /// When `RequestGroupMan` is available, iterates all groups and filters for
    /// completed, errored, or removed downloads using live `build_status_from_group`.
    /// Otherwise falls back to the `stopped_tasks` placeholder.
    pub async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let stopped: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.read().await;
                match g.status().await {
                    DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed => {
                        let gid_hex = gid.to_hex_string();
                        result.push(Self::build_status_from_group(&g, &gid_hex).await);
                    }
                    _ => {}
                }
            }
            result.into_iter().skip(offset).take(num).collect()
        } else {
            let stopped = self.stopped_tasks.read().await;
            stopped
                .iter()
                .skip(offset.min(stopped.len()))
                .take(num)
                .cloned()
                .collect()
        };
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(stopped)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.getGlobalStat` - Get global download statistics.
    ///
    /// Aggregates live speeds and counts from `RequestGroupMan` when available.
    pub async fn handle_global_stat(&self) -> JsonRpcResponse {
        let (dl_speed, ul_speed, active, waiting, stopped) =
            if let Some(group_man) = &self.group_man {
                let man = group_man.read().await;
                let mut dl = 0u64;
                let mut ul = 0u64;
                let mut active_n = 0usize;
                let mut waiting_n = 0usize;
                let mut stopped_n = 0usize;
                for (_, group_lock) in man.all_groups() {
                    let g = group_lock.read().await;
                    dl += g.get_download_speed_cached();
                    ul += g.get_uploaded_length();
                    match g.status().await {
                        DownloadStatus::Active => active_n += 1,
                        DownloadStatus::Waiting | DownloadStatus::Paused => waiting_n += 1,
                        DownloadStatus::Complete
                        | DownloadStatus::Error(_)
                        | DownloadStatus::Removed => stopped_n += 1,
                    }
                }
                (dl, ul, active_n, waiting_n, stopped_n)
            } else {
                let tasks = self.tasks.read().await;
                let (a, w): (Vec<_>, Vec<_>) =
                    tasks.values().partition(|s| s.status.status.is_active());
                (0, 0, a.len(), w.len(), 0)
            };
        let stat = GlobalStat {
            download_speed: dl_speed,
            upload_speed: ul_speed,
            num_active: active,
            num_waiting: waiting,
            num_stopped: stopped,
            num_stopped_total: self.num_stopped_total.load(Ordering::Relaxed),
        };
        JsonRpcResponse::success(serde_json::Value::Null, stat.to_json_value())
    }
}
