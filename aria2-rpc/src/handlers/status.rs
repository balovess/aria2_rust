//! Status query RPC handlers.
//!
//! Handlers for querying download status and global statistics.

use aria2_core::util::rwlock_ext::RwLockRecover;
use std::collections::HashSet;

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::types::{DownloadStatus, GlobalStat, StatusInfo};
use aria2_core::request::request_group::download_result::DownloadResult;

pub(crate) fn status_keys_for_request(
    req: &JsonRpcRequest,
    index: usize,
) -> Result<Vec<String>, JsonRpcError> {
    Ok(req
        .get_optional_param::<Vec<String>>(index)?
        .unwrap_or_default())
}

fn pagination_params(req: &JsonRpcRequest) -> Result<(i64, usize, Vec<String>), JsonRpcError> {
    let offset: i64 = req.get_param(0)?;
    let num: i64 = req.get_param(1)?;
    if num < 0 {
        return Err(JsonRpcError::RpcExecution(
            "num must be greater than or equal to 0".into(),
        ));
    }
    let num = usize::try_from(num)
        .map_err(|_| JsonRpcError::RpcExecution("num is out of range".into()))?;
    Ok((offset, num, status_keys_for_request(req, 2)?))
}

/// Apply aria2's pagination rules, including negative offsets.
fn paginate<T>(items: Vec<T>, offset: i64, num: usize) -> Vec<T> {
    if num == 0 {
        return Vec::new();
    }

    let size = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let originally_negative = offset < 0;
    let (start, count) = if originally_negative {
        let tempoffset = offset.saturating_add(size);
        if tempoffset < 0 {
            return Vec::new();
        }
        let num = i64::try_from(num).unwrap_or(i64::MAX);
        let mut start = tempoffset.saturating_sub(num.saturating_sub(1));
        let count = if start < 0 {
            start = 0;
            tempoffset.saturating_add(1)
        } else {
            num
        };
        (start, count)
    } else {
        if offset >= size {
            return Vec::new();
        }
        (offset, i64::try_from(num).unwrap_or(i64::MAX))
    };

    if start < 0 || start >= size {
        return Vec::new();
    }
    let end = start.saturating_add(count).min(size).max(start);
    let mut selected = items
        .into_iter()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect::<Vec<_>>();
    if originally_negative {
        selected.reverse();
    }
    selected
}

pub(crate) fn status_to_json(
    status: StatusInfo,
    keys: &[String],
) -> Result<serde_json::Value, JsonRpcError> {
    let mut value = serde_json::to_value(status)
        .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {e}")))?;
    if keys.is_empty() {
        return Ok(value);
    }

    let requested: HashSet<&str> = keys.iter().map(String::as_str).collect();
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|key, _| requested.contains(key.as_str()));
    }
    Ok(value)
}

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
            info = info.with_files(RpcEngine::build_file_infos_from_result(result));
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
        let keys = status_keys_for_request(req, 0)?;
        let active: Vec<StatusInfo> = if let Some(group_man) = self.group_man.as_ref() {
            let man = group_man.read().await;
            let mut result = Vec::new();
            for (gid, group_lock) in man.all_groups() {
                let g = group_lock.recover();
                if matches!(g.status(), DownloadStatus::Active) {
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
        let active = active
            .into_iter()
            .map(|status| status_to_json(status, &keys))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            active,
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
        let (offset, num, keys) = pagination_params(req)?;
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
            paginate(result, offset, num)
        } else {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan is not wired".into(),
            ));
        };
        let waiting = waiting
            .into_iter()
            .map(|status| status_to_json(status, &keys))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            waiting,
        ))
    }

    /// Handle `aria2.tellStopped` - List stopped/completed downloads with pagination.
    ///
    /// Iterates stopped download results and builds their status from core state.
    pub async fn handle_tell_stopped(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let (offset, num, keys) = pagination_params(req)?;
        let stopped: Vec<StatusInfo> = if let Some(group_man) = &self.group_man {
            let man = group_man.read().await;
            let results = paginate(man.get_stopped_results(0, usize::MAX), offset, num);
            results.iter().map(Self::build_status_from_result).collect()
        } else {
            return Err(JsonRpcError::InternalError(
                "RequestGroupMan is not wired".into(),
            ));
        };
        let stopped = stopped
            .into_iter()
            .map(|status| status_to_json(status, &keys))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            stopped,
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
