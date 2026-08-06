//! Status query RPC handlers.
//!
//! Handlers for querying download status and global statistics.

use aria2_core::util::rwlock_ext::RwLockRecover;

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, GlobalStat, StatusInfo};
use aria2_core::request::request_group::download_result::DownloadResult;

impl RpcEngine {
    pub(crate) fn build_status_from_result(result: &DownloadResult) -> StatusInfo {
        let mut info = StatusInfo::new(result.gid_hex())
            .with_status(result.status.clone())
            .with_total_length(result.total_length)
            .with_completed_length(result.completed_length)
            .with_upload_length(result.upload_length)
            .with_download_speed(result.download_speed)
            .with_upload_speed(result.upload_speed)
            .with_error_code(result.code.as_code() as i32)
            .with_error_message(result.message.clone())
            .with_dir(result.dir.clone());
        if !result.files.is_empty() {
            info = info.with_files(
                result
                    .files
                    .iter()
                    .map(|file| {
                        crate::types::FileInfo::new(file.path.clone(), file.length)
                            .with_completed(file.completed_length)
                            .with_index(file.index)
                    })
                    .collect(),
            );
        }
        info
    }

    /// Handle `aria2.tellActive` - List all active/running downloads.
    ///
    /// Iterates all registered groups and reads live progress from their atomic fields.
    pub async fn handle_tell_active(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let active: Vec<StatusInfo> = if let Some(group_man) = self.group_man.as_ref() {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.recover();
                if g.status().is_active() {
                    let gid_hex = gid.to_hex_string();
                    result.push(Self::build_status_from_group(&g, &gid_hex));
                }
            }
            result
        } else {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan is not wired".into(),
            ));
        };
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(active)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.tellWaiting` - List waiting/queued downloads with pagination.
    ///
    /// Per original C++ aria2 behaviour, paused downloads are included in
    /// the waiting list (they live in `reservedGroups_`). The `status`
    /// field of each entry distinguishes "waiting" from "paused".
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
                let g = group_lock.recover();
                // Original aria2: reservedGroups_ contains both waiting
                // and paused downloads. tellWaiting returns both.
                match g.status() {
                    DownloadStatus::Waiting | DownloadStatus::Paused => {
                        let gid_hex = gid.to_hex_string();
                        result.push(Self::build_status_from_group(&g, &gid_hex));
                    }
                    _ => {}
                }
            }
            result.into_iter().skip(offset).take(num).collect()
        } else {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan is not wired".into(),
            ));
        };
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            serde_json::to_value(waiting)
                .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
    ///
    /// Iterates stopped download results and builds their status from core state.
    pub async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let offset: usize = req.get_param_or_default(0);
        let num: usize = req.get_param_or_default(1);
        let stopped: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            man.get_stopped_results(offset as i32, num)
                .iter()
                .map(Self::build_status_from_result)
                .collect()
        } else {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan is not wired".into(),
            ));
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
    pub async fn handle_global_stat(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let (dl_speed, ul_speed, active, waiting, stopped) =
            if let Some(group_man) = self.group_man.as_ref() {
                let man = group_man.read().await;
                let mut dl = 0u64;
                let mut ul = 0u64;
                let mut active_n = 0usize;
                let mut waiting_n = 0usize;
                let stopped_n = man.stopped_results_len();
                for (_, group_lock) in man.all_groups() {
                    let g = group_lock.recover();
                    dl += g.get_download_speed_cached();
                    ul += g.get_upload_speed_cached();
                    match g.status() {
                        DownloadStatus::Active => active_n += 1,
                        DownloadStatus::Waiting | DownloadStatus::Paused => waiting_n += 1,
                        DownloadStatus::Complete
                        | DownloadStatus::Error(_)
                        | DownloadStatus::Removed => {}
                    }
                }
                (dl, ul, active_n, waiting_n, stopped_n)
            } else {
                (0, 0, 0, 0, 0)
            };
        let stat = GlobalStat {
            download_speed: dl_speed,
            upload_speed: ul_speed,
            num_active: active,
            num_waiting: waiting,
            num_stopped: stopped,
            num_stopped_total: stopped,
        };
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), stat.to_json_value())
    }
}
