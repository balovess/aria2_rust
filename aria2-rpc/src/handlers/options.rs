//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

use aria2_core::RUNTIME_CHANGEABLE_OPTIONS;

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// Valid option keys accepted by `aria2.changeOption`.
const VALID_OPTION_KEYS: &[&str] = &[
    "split", "max-connection-per-server", "max-download-limit",
    "max-upload-limit", "dir", "out", "seed-time", "seed-ratio",
    // File allocation
    "file-allocation", "mmap-threshold", "secure-falloc",
    // Checksum & cookies
    "checksum", "cookie-file", "cookies",
    // BitTorrent
    "bt-force-encrypt", "bt-require-crypto", "enable-dht",
    "dht-listen-port", "dht-entry-point", "enable-public-trackers",
    "bt-piece-selection-strategy", "bt-endgame-threshold",
    "bt-max-upload-slots", "bt-optimistic-unchoke-interval", "bt-snubbed-timeout",
    "bt-prioritize-piece", "enable-utp", "utp-listen-port",
    // Retry
    "max-retries", "retry-wait",
    // DHT
    "dht-file-path",
    // Proxy
    "http-proxy", "all-proxy", "https-proxy", "ftp-proxy", "no-proxy",
    // HTTP headers
    "header", "user-agent", "referer",
];

impl RpcEngine {
    /// Handle `aria2.getGlobalOption` - Get global configuration options.
    pub async fn handle_get_global_option(&self) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        JsonRpcResponse::success(
            serde_json::Value::Null,
            serde_json::to_value(&*opts).unwrap_or(serde_json::json!({})),
        )
    }

    /// Handle `aria2.changeGlobalOption` - Modify global configuration options.
    pub async fn handle_change_global_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;
        let mut opts = self.global_opts.write().await;
        for (k, v) in new_opts {
            opts.insert(k, v);
        }
        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }

    /// Handle `aria2.getOption` - Get per-task options.
    ///
    /// Resolution order:
    /// 1. Per-task options stored via `aria2.changeOption` (returned as-is).
    /// 2. If no per-task overrides exist but the task is registered in the
    ///    shared `RequestGroupMan`, fall back to the current global options.
    /// 3. Otherwise return `MethodNotFound`.
    pub async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Step 1: per-task overrides.
        let task_opts = self.task_opts.read().await;
        if let Some(opts) = task_opts.get(&gid) {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(opts).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            ));
        }
        // Release the task_opts read lock before awaiting on group_man to
        // avoid holding it across an await point and keep lock hold times short.
        drop(task_opts);

        // Step 2: fall back to global options if the task is known to the
        // shared RequestGroupMan.
        if let Some(group_man) = self.group_man.as_ref() {
            let task_exists = group_man.read().await.group_by_hex(&gid).is_some();
            if task_exists {
                let global_opts = self.global_opts.read().await;
                return Ok(JsonRpcResponse::success(
                    req.id.clone().unwrap_or_default(),
                    serde_json::to_value(&*global_opts).map_err(|e| {
                        JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                    })?,
                ));
            }
        }

        // Step 3: task does not exist anywhere.
        Err(JsonRpcError::MethodNotFound(format!(
            "GID {} not found",
            gid
        )))
    }

    /// Handle `aria2.changeOption` - Modify per-task options.
    ///
    /// Only runtime-changeable options (see [`RUNTIME_CHANGEABLE_OPTIONS`])
    /// may be modified via this method; startup-only options are rejected
    /// with `InvalidParams`. Accepted changes are:
    ///
    /// 1. Propagated to the running `RequestGroup` via `RequestGroupMan`
    ///    (when the engine is wired to a live download engine), so they take
    ///    effect immediately on the in-flight download.
    /// 2. Stored in `task_opts` so `aria2.getOption` returns the current
    ///    values and so they persist across session reloads.
    pub async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;

        // Step 1: reject unknown option keys entirely.
        for key in changes.keys() {
            if !VALID_OPTION_KEYS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!("Unknown option: {}", key)));
            }
        }

        // Step 2: reject startup-only options. Per spec, only runtime-changeable
        // options may be modified via aria2.changeOption on a live task.
        for key in changes.keys() {
            if !RUNTIME_CHANGEABLE_OPTIONS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!(
                    "Option '{}' cannot be changed at runtime",
                    key
                )));
            }
        }

        // Step 3: propagate to the running RequestGroup (if any). The GID may
        // not be known to RequestGroupMan yet (e.g., the task was added via
        // RPC but the download has not started); in that case we still store
        // the change in task_opts so it applies when the task starts.
        // `changes` is cloned because we also consume it for task_opts below.
        if let Some(ref group_man) = self.group_man {
            let gm = group_man.read().await;
            if let Err(e) = gm.update_group_options(&gid, changes.clone()).await {
                // "not found" means the task isn't registered in group_man yet
                // — that's acceptable, the change will apply from task_opts on
                // start. Any other error is propagated as InvalidParams.
                if !e.contains("not found") {
                    return Err(JsonRpcError::InvalidParams(e));
                }
                tracing::debug!(
                    "changeOption for GID {} not applied to a running group (not registered yet); storing in task_opts only",
                    gid
                );
            }
        }

        // Step 4: persist in task_opts for getOption retrieval and session
        // reload. This runs after propagation so a failed propagate (non-
        // not-found error) returns early without mutating task_opts.
        let mut task_opts = self.task_opts.write().await;
        let entry = task_opts.entry(gid).or_insert_with(HashMap::new);
        for (k, v) in changes {
            entry.insert(k, v);
        }
        drop(task_opts);

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }
}
