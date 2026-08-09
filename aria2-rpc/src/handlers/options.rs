//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

use aria2_core::config::{OptionRegistry, is_global_option_changeable};
use aria2_core::request::request_group::{RequestGroup, is_option_changeable};
use aria2_core::util::rwlock_ext::RwLockRecover;

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::rpc_helpers::normalize_rpc_options;

/// Options that can be changed at runtime via `aria2.changeGlobalOption`.
///
/// Extracted from the C++ aria2 `OptionHandlerFactory.cc` — all options
/// with `setChangeGlobalOption(true)`. Keep in sync with the original.
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

fn parse_rate_limit_for_option_change(
    value: &serde_json::Value,
    option: &str,
) -> Result<Option<u64>, JsonRpcError> {
    parse_rate_limit(value, option).map_err(|error| match error {
        JsonRpcError::InvalidParams(message) => JsonRpcError::RpcExecution(message),
        other => other,
    })
}

fn parse_non_negative_u64_for_option_change(
    value: &serde_json::Value,
    option: &str,
) -> Result<u64, JsonRpcError> {
    parse_non_negative_u64(value, option).map_err(|error| match error {
        JsonRpcError::InvalidParams(message) => JsonRpcError::RpcExecution(message),
        other => other,
    })
}

fn validate_registered_option(
    registry: &OptionRegistry,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), JsonRpcError> {
    if registry.get(key).is_none() {
        return Ok(());
    }
    registry
        .parse_rpc_value(key, value)
        .map(|_| ())
        .map_err(|error| JsonRpcError::RpcExecution(format!("Option '{}': {}", key, error)))
}

impl RpcEngine {
    /// Handle `aria2.getGlobalOption` - Get global configuration options.
    ///
    /// Per C++ aria2, `rpc-secret` (PREF_RPC_SECRET) is excluded from the
    /// output so that the secret is never leaked to RPC clients.
    pub async fn handle_get_global_option(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        let mut value =
            serde_json::to_value(normalize_rpc_options(&opts)).unwrap_or(serde_json::json!({}));
        // Strip rpc-secret matching C++ aria2 behaviour
        if let Some(map) = value.as_object_mut() {
            map.remove("rpc-secret");
        }
        JsonRpcResponse::success(req.id.clone().unwrap_or_default(), value)
    }

    /// Handle `aria2.changeGlobalOption` - Modify global configuration options.
    ///
    /// Per original C++ aria2, only options with `setChangeGlobalOption(true)`
    /// in the OptionHandler may be modified via this method. Unknown and
    /// non-changeable option keys are ignored, matching C++ aria2.
    pub async fn handle_change_global_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.get_param(0)?;
        // C++ aria2 silently ignores unknown and non-changeable options.
        let new_opts: HashMap<String, serde_json::Value> = new_opts
            .into_iter()
            .filter(|(key, _)| is_global_option_changeable(key))
            .collect();

        // Parse before mutating shared state. C++ propagates option handler
        // parse failures as execution errors, and a failed request must not
        // leave an invalid value visible through getGlobalOption.
        let registry = OptionRegistry::new();
        for (key, value) in &new_opts {
            match key.as_str() {
                "max-overall-download-limit" | "max-overall-upload-limit" => {
                    parse_rate_limit_for_option_change(value, key)?;
                }
                // aria2 treats zero as the unlimited value in the runtime
                // engine even though the static registry uses a positive
                // command-line lower bound for this option.
                "max-concurrent-downloads" => {
                    let value = parse_non_negative_u64_for_option_change(value, key)?;
                    u32::try_from(value).map_err(|_| {
                        JsonRpcError::RpcExecution(format!("Option '{}' is too large", key))
                    })?;
                }
                _ => validate_registered_option(&registry, key, value)?,
            }
        }

        let current_download_limit = self
            .global_opts
            .read()
            .await
            .get("max-overall-download-limit")
            .map(|value| parse_rate_limit_for_option_change(value, "max-overall-download-limit"))
            .transpose()?;
        let current_upload_limit = self
            .global_opts
            .read()
            .await
            .get("max-overall-upload-limit")
            .map(|value| parse_rate_limit_for_option_change(value, "max-overall-upload-limit"))
            .transpose()?;

        let mut opts = self.global_opts.write().await;
        for (k, v) in &new_opts {
            opts.insert(k.clone(), v.clone());
        }
        drop(opts);
        // Apply engine-level options live.
        // max-concurrent-downloads drives the engine's slot limit; the
        // engine loop reduces excess active downloads immediately.
        if let Some(value) = new_opts.get("max-concurrent-downloads")
            && let Some(tx) = &self.engine_cmd_tx
        {
            let max = parse_non_negative_u64_for_option_change(value, "max-concurrent-downloads")?;
            use aria2_core::engine::engine_command::EngineCommand;
            let max = u32::try_from(max).map_err(|_| {
                JsonRpcError::RpcExecution(
                    "Option 'max-concurrent-downloads' is too large".to_string(),
                )
            })?;
            let _ = tx.send(EngineCommand::SetMaxConcurrent { max });
        }
        if new_opts.contains_key("max-overall-download-limit")
            || new_opts.contains_key("max-overall-upload-limit")
        {
            let download_limit = match new_opts.get("max-overall-download-limit") {
                Some(value) => {
                    parse_rate_limit_for_option_change(value, "max-overall-download-limit")?
                }
                None => current_download_limit.flatten(),
            };
            let upload_limit = match new_opts.get("max-overall-upload-limit") {
                Some(value) => {
                    parse_rate_limit_for_option_change(value, "max-overall-upload-limit")?
                }
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
    /// 1. The task snapshot captured when `aria2.add*` created the group,
    ///    overlaid with options already applied through `aria2.changeOption`.
    /// 2. Legacy group-only adapters without a task snapshot use their applied
    ///    options, then the current global options as their historical fallback.
    /// 3. Otherwise return an execution error.
    pub async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Pending changes are intentionally absent until the group restarts;
        // this matches C++ getOption rather than exposing a queued value.
        let group_option_state = if let Some(group_man) = self.group_man.as_ref() {
            let group_man = group_man.read().await;
            group_man
                .group_by_hex(&gid)
                .map(|group| {
                    let group = group.recover();
                    (
                        group.effective_option_snapshot(),
                        group.runtime_options(),
                    )
                })
        } else {
            None
        };
        let group_exists = group_option_state.is_some();

        // Live groups own their option snapshot in core. This makes CLI,
        // session-restored, and RPC-created tasks follow the same rule.
        if let Some((Some(options), _)) = group_option_state.as_ref() {
            let wire_opts = normalize_rpc_options(options);
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(wire_opts).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            ));
        }

        // Stopped-result and legacy adapter fallback. A live RequestGroup is
        // always preferred above because C++ reads group->getOption().
        let task_opts = self.task_opts.read().await;
        if let Some(snapshot) = task_opts.get(&gid) {
            let wire_opts = normalize_rpc_options(snapshot);
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(wire_opts).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            ));
        }
        drop(task_opts);

        // Preserve the legacy group-only adapter behavior for callers that
        // registered a RequestGroup without the RPC task snapshot.
        if let Some((_, runtime_options)) = group_option_state
            && !runtime_options.is_empty()
        {
            let wire_opts = normalize_rpc_options(&runtime_options);
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(wire_opts).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            ));
        }

        if group_exists {
            let global_opts = self.global_opts.read().await;
            let wire_opts = normalize_rpc_options(&global_opts);
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serde_json::to_value(wire_opts).map_err(|e| {
                    JsonRpcError::InternalError(format!("Serialization failed: {}", e))
                })?,
            ));
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
    ///   Other options are ignored.
    /// - For **reserved/waiting** downloads: options with
    ///   `setChangeOptionForReserved(true)` take effect immediately.
    ///   Other options are ignored.
    pub async fn handle_change_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;
        let raw_changes: HashMap<String, serde_json::Value> = req.get_param(1)?;
        let changes = normalize_rpc_options(&raw_changes);

        if let Some(group_man) = self.group_man.as_ref() {
            let manager = group_man.read().await;
            manager
                .change_group_options(&gid, changes)
                .map_err(JsonRpcError::RpcExecution)?;
        } else {
            // Keep the un-wired fixture path usable for legacy unit callers.
            let mut immediate = HashMap::new();
            for (key, value) in changes {
                if matches!(
                    is_option_changeable(&key, false),
                    aria2_core::config::ChangeableKind::Immediate
                ) && RequestGroup::validate_option_update(&key, &value)
                    .map_err(JsonRpcError::RpcExecution)?
                {
                    immediate.insert(key, value);
                }
            }
            if !immediate.is_empty() {
                self.task_opts
                    .write()
                    .await
                    .entry(gid)
                    .or_default()
                    .extend(immediate);
            }
        }

        Ok(JsonRpcResponse::success(
            req.id.clone().unwrap_or_default(),
            "OK",
        ))
    }
}
