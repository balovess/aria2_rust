//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

use aria2_core::request::request_group::{ChangeableKind, is_option_changeable};

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// Options that can be changed at runtime via `aria2.changeGlobalOption`.
///
/// Extracted from the C++ aria2 `OptionHandlerFactory.cc` — all options
/// with `setChangeGlobalOption(true)`. Keep in sync with the original.
const RUNTIME_GLOBAL_CHANGEABLE_OPTIONS: &[&str] = &[
    // General
    "allow-overwrite",
    "allow-piece-length-change",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
    "check-integrity",
    "conditional-get",
    "continue",
    "dir",
    "download-result",
    "enable-mmap",
    "file-allocation",
    "force-save",
    "save-not-found",
    "hash-check-only",
    "keep-unfinished-download-result",
    "max-concurrent-downloads",
    "max-download-limit",
    "max-overall-download-limit",
    "max-overall-upload-limit",
    "max-upload-limit",
    "min-split-size",
    "no-conf",
    "optimize-concurrent-downloads",
    "preview",
    "reuse-uri",
    "save-session-interval",
    "server-stat-if",
    "server-stat-of",
    "split",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    "use-server-stat",
    // HTTP/FTP
    "connect-timeout",
    "dry-run",
    "lowest-speed-limit",
    "max-connection-per-server",
    "max-file-not-found",
    "max-tries",
    "no-netrc",
    "proxy-method",
    "retry-wait",
    "ftp-type",
    "ftp-reuse-connection",
    "http-auth-challenge",
    "http-no-cache",
    "http-user",
    "http-passwd",
    "http-proxy",
    "https-proxy",
    "ftp-proxy",
    "all-proxy",
    "no-proxy",
    "user-agent",
    "referer",
    "header",
    "cookie-file",
    "cookies",
    // BitTorrent
    "bt-detach-seed-only",
    "bt-enable-hook-after-hash-check",
    "bt-enable-lpd",
    "bt-force-encrypt",
    "bt-hash-check-seed",
    "bt-load-saved-metadata",
    "bt-max-peers",
    "bt-max-upload-slots",
    "bt-min-crypto-level",
    "bt-prioritize-piece",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "bt-require-crypto",
    "bt-save-metadata",
    "bt-seed-unverified",
    "bt-stop-timeout",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "dht-file-path",
    "dht-file-path6",
    "dht-listen-port",
    "dht-entry-point",
    "enable-dht",
    "enable-dht6",
    "enable-public-trackers",
    "enable-utp",
    "follow-torrent",
    "listen-port",
    "max-overall-upload-limit",
    "peer-agent",
    "peer-id-prefix",
    "seed-ratio",
    "seed-time",
    "utp-listen-port",
    // Metalink
    "follow-metalink",
    "metalink-preferred-protocol",
    "metalink-version",
    "metalink-language",
    "metalink-os",
    // RPC
    "rpc-max-request-size",
    // Checksum
    "checksum",
    // Advanced
    "auto-save-interval",
    "disk-cache",
    "follow-torrent",
    "max-download-result",
    "no-file-allocation-limit",
    "piece-length",
    "show-console-readout",
    "show-files",
];

impl RpcEngine {
    /// Handle `aria2.getGlobalOption` - Get global configuration options.
    ///
    /// Per C++ aria2, `rpc-secret` (PREF_RPC_SECRET) is excluded from the
    /// output so that the secret is never leaked to RPC clients.
    pub async fn handle_get_global_option(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        let mut value = serde_json::to_value(&*opts).unwrap_or(serde_json::json!({}));
        // Strip rpc-secret matching C++ aria2 behaviour
        if let Some(map) = value.as_object_mut() {
            map.remove("rpc-secret");
        }
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), value)
    }

    /// Handle `aria2.changeGlobalOption` - Modify global configuration options.
    ///
    /// Per original C++ aria2, only options with `setChangeGlobalOption(true)`
    /// in the OptionHandler may be modified via this method. Unknown or
    /// non-changeable option keys are rejected with `InvalidParams`.
    pub async fn handle_change_global_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;

        // Validate: only runtime-changeable global options are accepted.
        for key in new_opts.keys() {
            if !RUNTIME_GLOBAL_CHANGEABLE_OPTIONS.contains(&key.as_str()) {
                return Err(JsonRpcError::InvalidParams(format!(
                    "Option '{}' cannot be changed via changeGlobalOption",
                    key
                )));
            }
        }

        let mut opts = self.global_opts.write().await;
        for (k, v) in &new_opts {
            opts.insert(k.clone(), v.clone());
        }
        drop(opts);
        // Track user-set options separately so addUri/addTorrent can apply
        // them to subsequent downloads (registry defaults must not leak in).
        {
            let mut user = self.user_global_opts.write().await;
            for (k, v) in &new_opts {
                user.insert(k.clone(), v.clone());
            }
        }
        // Apply engine-level options live.
        // max-concurrent-downloads drives the engine's slot limit; the
        // engine loop reduces excess active downloads immediately.
        if let Some(max) = new_opts
            .get("max-concurrent-downloads")
            .and_then(|v| v.as_u64())
        {
            if let Some(tx) = &self.engine_cmd_tx {
                use aria2_core::engine::engine_command::EngineCommand;
                let _ = tx.send(EngineCommand::SetMaxConcurrent {
                    max: max as u32,
                });
            }
        }
        // TODO(engine): max-overall-download-limit / max-overall-upload-limit
        // need a global RateLimiter in the engine (per-download limits already
        // work via max-download-limit / max-upload-limit).
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
        Err(JsonRpcError::RpcExecution(format!(
            "GID {} not found",
            gid
        )))
    }

    /// Handle `aria2.changeOption` - Modify per-task options.
    ///
    /// Matches C++ `ChangeOptionRpcMethod::process()`:
    /// - For **active** downloads: only options with `setChangeOption(true)`
    ///   take effect immediately. Options with `setChangeOptionForReserved(true)`
    ///   are stored as "pending" and applied when the download is paused/restarted.
    ///   Other options are rejected with `InvalidParams`.
    /// - For **reserved/waiting** downloads: options with
    ///   `setChangeOptionForReserved(true)` take effect immediately.
    ///   Other options are rejected with `InvalidParams`.
    pub async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let changes: HashMap<String, serde_json::Value> = req.get_param(1)?;

        // Determine whether the download is active. If we can't find it in
        // group_man, assume it's reserved (not yet started) — this is safe
        // because options for reserved downloads are a superset.
        let is_active = if let Some(ref group_man) = self.group_man {
            let gm = group_man.read().await;
            gm.is_group_active(&gid).unwrap_or(false)
        } else {
            false
        };

        // Classify each option key and partition into immediate/pending.
        let mut immediate = HashMap::new();
        let mut pending = HashMap::new();
        for (key, value) in &changes {
            match is_option_changeable(key.as_str(), is_active) {
                ChangeableKind::Immediate => {
                    immediate.insert(key.clone(), value.clone());
                }
                ChangeableKind::Pending => {
                    pending.insert(key.clone(), value.clone());
                }
                ChangeableKind::NotChangeable => {
                    return Err(JsonRpcError::InvalidParams(format!(
                        "Option '{}' cannot be changed via changeOption",
                        key
                    )));
                }
            }
        }

        // Apply immediate options to the running RequestGroup.
        if !immediate.is_empty()
            && let Some(ref group_man) = self.group_man {
                let gm = group_man.read().await;
                if let Err(e) = gm.update_group_options(&gid, immediate.clone()) {
                    if !e.contains("not found") {
                        return Err(JsonRpcError::InvalidParams(e));
                    }
                    tracing::debug!(
                        "changeOption for GID {} not applied to a running group (not registered yet); storing in task_opts only",
                        gid
                    );
                }
            }

        // Store pending options (to be applied on next pause/restart).
        // In C++ aria2, these trigger a pause + restart cycle.
        if !pending.is_empty() {
            tracing::info!(
                "GID {}: {} options stored as pending (applied on pause/restart): {:?}",
                gid,
                pending.len(),
                pending.keys().collect::<Vec<_>>()
            );
            // TODO: Implement pause + restart mechanism matching C++:
            //   group->setPendingOption(pendingOption);
            //   if (pauseRequestGroup(group, false, false)) {
            //       group->setRestartRequested(true);
            //   }
        }

        // Persist all changes in task_opts for getOption retrieval and
        // session reload.
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
