//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

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
    pub async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let task_opts = self.task_opts.read().await;
        match task_opts.get(&gid) {
            Some(opts) => Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(opts)
                    .map_err(|e| JsonRpcError::InternalError(format!("Serialization failed: {}", e)))?,
            )),
            None => Err(JsonRpcError::MethodNotFound(format!(
                "GID {} not found",
                gid
            ))),
        }
    }

    /// Handle `aria2.changeOption` - Modify per-task options.
    pub async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;

        for key in changes.keys() {
            if !VALID_OPTION_KEYS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!("Unknown option: {}", key)));
            }
        }

        // Clone the GID before moving it into `entry(gid)` so it remains
        // available for the running-task check below.
        let gid_for_check = gid.clone();
        let mut task_opts = self.task_opts.write().await;
        let entry = task_opts.entry(gid).or_insert_with(HashMap::new);
        for (k, v) in changes {
            entry.insert(k, v);
        }
        // Release the task_opts write lock before acquiring the tasks read
        // lock to keep lock hold times short and avoid any lock-ordering risk.
        drop(task_opts);

        // Warn if the option change targets a currently running task: the
        // update is persisted to task_opts but will only take effect on the
        // next session load, not on the in-flight download.
        if self.tasks.read().await.contains_key(&gid_for_check) {
            tracing::warn!(
                "Option change for running task {} will take effect on next session load, not on the current download",
                gid_for_check
            );
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }
}
