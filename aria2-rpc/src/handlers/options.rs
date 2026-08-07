//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

use aria2_core::request::request_group::{ChangeableKind, is_option_changeable};
use aria2_core::util::rwlock_ext::RwLockRecover;

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

fn parse_non_negative_u64(value: &serde_json::Value, option: &str) -> Result<u64, JsonRpcError> {
    if let Some(value) = value.as_u64() {
        return Ok(value);
    }
    if let Some(value) = value.as_i64() {
        return u64::try_from(value).map_err(|_| {
            JsonRpcError::InvalidParams(format!("Option '{}' must be non-negative", option))
        });
    }
    if let Some(value) = value.as_str() {
        return value.trim().parse::<u64>().map_err(|_| {
            JsonRpcError::InvalidParams(format!("Option '{}' must be an integer", option))
        });
    }
    Err(JsonRpcError::InvalidParams(format!(
        "Option '{}' must be an integer",
        option
    )))
}

fn parse_rate_limit(value: &serde_json::Value, option: &str) -> Result<Option<u64>, JsonRpcError> {
    let raw = if let Some(value) = value.as_str() {
        value.trim().to_string()
    } else {
        parse_non_negative_u64(value, option)?.to_string()
    };
    let (number, multiplier) = match raw.chars().last() {
        Some('k' | 'K') => (&raw[..raw.len() - 1], 1024u64),
        Some('m' | 'M') => (&raw[..raw.len() - 1], 1024u64 * 1024),
        Some('g' | 'G') => (&raw[..raw.len() - 1], 1024u64 * 1024 * 1024),
        Some('t' | 'T') => (&raw[..raw.len() - 1], 1024u64 * 1024 * 1024 * 1024),
        _ => (raw.as_str(), 1),
    };
    let number = number.parse::<f64>().map_err(|_| {
        JsonRpcError::InvalidParams(format!("Option '{}' must be a byte rate", option))
    })?;
    if !number.is_finite() || number < 0.0 {
        return Err(JsonRpcError::InvalidParams(format!(
            "Option '{}' must be a non-negative byte rate",
            option
        )));
    }
    let bytes = number * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return Err(JsonRpcError::InvalidParams(format!(
            "Option '{}' is too large",
            option
        )));
    }
    let bytes = bytes as u64;
    Ok((bytes > 0).then_some(bytes))
}

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

        let current_download_limit = self
            .global_opts
            .read()
            .await
            .get("max-overall-download-limit")
            .map(|value| parse_rate_limit(value, "max-overall-download-limit"))
            .transpose()?;
        let current_upload_limit = self
            .global_opts
            .read()
            .await
            .get("max-overall-upload-limit")
            .map(|value| parse_rate_limit(value, "max-overall-upload-limit"))
            .transpose()?;

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
        if let Some(value) = new_opts.get("max-concurrent-downloads")
            && let Some(tx) = &self.engine_cmd_tx
        {
            let max = parse_non_negative_u64(value, "max-concurrent-downloads")?;
            use aria2_core::engine::engine_command::EngineCommand;
            let max = u32::try_from(max).map_err(|_| {
                JsonRpcError::InvalidParams(
                    "Option 'max-concurrent-downloads' is too large".to_string(),
                )
            })?;
            let _ = tx.send(EngineCommand::SetMaxConcurrent { max });
        }
        if new_opts.contains_key("max-overall-download-limit")
            || new_opts.contains_key("max-overall-upload-limit")
        {
            let download_limit = match new_opts.get("max-overall-download-limit") {
                Some(value) => parse_rate_limit(value, "max-overall-download-limit")?,
                None => current_download_limit.flatten(),
            };
            let upload_limit = match new_opts.get("max-overall-upload-limit") {
                Some(value) => parse_rate_limit(value, "max-overall-upload-limit")?,
                None => current_upload_limit.flatten(),
            };
            if let Some(tx) = &self.engine_cmd_tx {
                use aria2_core::engine::engine_command::EngineCommand;
                let _ = tx.send(EngineCommand::SetGlobalRateLimit {
                    download_limit,
                    upload_limit,
                });
            }
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
        if let Some(opts) = task_opts.get(&gid)
            && !opts.is_empty()
        {
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
        Err(JsonRpcError::RpcExecution(format!("GID {} not found", gid)))
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

        let group = if let Some(group_man) = self.group_man.as_ref() {
            let group_man = group_man.read().await;
            group_man.group_by_hex(&gid)
        } else {
            None
        };
        if self.group_man.is_some()
            && group.is_none()
            && !self.task_opts.read().await.contains_key(&gid)
            && changes.keys().any(|key| {
                matches!(
                    is_option_changeable(key.as_str(), false),
                    ChangeableKind::NotChangeable
                )
            })
        {
            return Err(JsonRpcError::RpcExecution(format!("GID {} not found", gid)));
        }
        let is_active = group
            .as_ref()
            .is_some_and(|group| group.recover().status().is_active());

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
            && let Some(ref group_man) = self.group_man
        {
            let gm = group_man.write().await;
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
            if let Some(group) = group.clone() {
                {
                    let group_guard = group.recover();
                    group_guard.set_pending_options(pending);
                }
                if is_active {
                    let group_gid = group.recover().gid();
                    let gm = self.group_man.as_ref().unwrap().read().await;
                    gm.pause_group(group_gid)
                        .map_err(|e| JsonRpcError::RpcExecution(e.to_string()))?;
                    group.recover().request_restart();
                }
            }
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
