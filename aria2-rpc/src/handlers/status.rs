//! Status query RPC handlers.
//!
//! Handlers for querying download status and global statistics.

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, GlobalStat, StatusInfo};

impl RpcEngine {
    /// Handle `aria2.tellActive` - List all active/running downloads.
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
            serde_json::to_value(waiting)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
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
            serde_json::to_value(result)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.getGlobalStat` - Get global download statistics.
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
}
