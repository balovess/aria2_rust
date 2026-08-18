//! Option management RPC handlers.
//!
//! Handlers for getting and changing download options (per-task and global).

use std::collections::HashMap;

use aria2_core::config::{OptionRegistry, is_global_option_changeable, project_initial_options};
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

fn serialize_request_options(
    options: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, JsonRpcError> {
    let projected = project_initial_options(
        options
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    serde_json::to_value(normalize_rpc_options(&projected))
        .map_err(|error| JsonRpcError::InternalError(format!("Serialization failed: {error}")))
}

impl RpcEngine {
    /// Handle `aria2.getGlobalOption` - Get global configuration options.
    ///
    /// C++ aria2 returns every defined option with an `OptionHandler`, except
    /// `rpc-secret`. The core registry owns that visibility policy so hidden
    /// original options remain observable while Rust-only extensions do not
    /// change the original wire response.
    pub async fn handle_get_global_option(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let opts = self.global_opts.read().await;
        let registry = OptionRegistry::new();
        let projected = registry.project_defined_global_options_for_rpc(&opts);
        let value = serde_json::to_value(normalize_rpc_options(&projected))
            .unwrap_or(serde_json::json!({}));
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
        self.handle_change_global_option_values(req.id.clone(), new_opts)
            .await
    }

    /// Owned network path for `aria2.changeGlobalOption`.
    pub(crate) async fn handle_change_global_option_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let new_opts: HashMap<String, serde_json::Value> = req.take_param(0)?;
        self.handle_change_global_option_values(req.id.clone(), new_opts)
            .await
    }

    async fn handle_change_global_option_values(
        &self,
        request_id: Option<serde_json::Value>,
        new_opts: HashMap<String, serde_json::Value>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
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
        #[cfg(feature = "bittorrent")]
        if let Some(value) = new_opts.get("bt-tracker-source")
            && let Some(tx) = &self.engine_cmd_tx
        {
            use aria2_core::engine::engine_command::EngineCommand;
            use aria2_core::request::request_group::option_value_to_string;

            let sources = option_value_to_string(value).ok_or_else(|| {
                JsonRpcError::RpcExecution(
                    "Option 'bt-tracker-source' must be a string or array".to_string(),
                )
            })?;
            if sources
                .split([',', '\n'])
                .map(str::trim)
                .all(|source| source.is_empty())
            {
                return Err(JsonRpcError::RpcExecution(
                    "Option 'bt-tracker-source' must contain at least one source".to_string(),
                ));
            }
            let _ = tx.send(EngineCommand::SetPublicTrackerSources { sources });
        }
        #[cfg(feature = "bittorrent")]
        if let Some(value) = new_opts.get("bt-tracker-update-interval")
            && let Some(tx) = &self.engine_cmd_tx
        {
            use aria2_core::engine::engine_command::EngineCommand;

            let seconds =
                parse_non_negative_u64_for_option_change(value, "bt-tracker-update-interval")?;
            if seconds == 0 {
                return Err(JsonRpcError::RpcExecution(
                    "Option 'bt-tracker-update-interval' must be greater than zero".to_string(),
                ));
            }
            let _ = tx.send(EngineCommand::SetPublicTrackerUpdateInterval { seconds });
        }
        #[cfg(feature = "bittorrent")]
        if let Some(value) = new_opts.get("enable-public-trackers")
            && let Some(tx) = &self.engine_cmd_tx
        {
            use aria2_core::engine::engine_command::EngineCommand;

            let enabled = registry
                .parse_rpc_value("enable-public-trackers", value)
                .map_err(JsonRpcError::RpcExecution)?
                .as_bool()
                .ok_or_else(|| {
                    JsonRpcError::RpcExecution(
                        "Option 'enable-public-trackers' must be a boolean".to_string(),
                    )
                })?;
            let _ = tx.send(EngineCommand::SetPublicTrackersEnabled { enabled });
        }
        Ok(JsonRpcResponse::success(
            request_id.unwrap_or_default(),
            "OK",
        ))
    }

    /// Handle `aria2.getOption` - Get per-task options.
    ///
    /// Resolution order:
    /// 1. A live group returns its creation snapshot overlaid with options
    ///    already applied through `aria2.changeOption`.
    /// 2. A stopped result returns the same snapshot persisted by core.
    /// 3. Legacy groups without a snapshot use their applied options, then the
    ///    current global options as their historical fallback.
    /// 4. Otherwise return an execution error.
    pub async fn handle_get_option(
        &self,
        req: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.get_param(0)?;

        // Pending changes are intentionally absent until the group restarts;
        // this matches C++ getOption rather than exposing a queued value.
        let (group_option_state, stopped_result) = if let Some(group_man) = self.group_man.as_ref()
        {
            let group_option_state = group_man.group_by_hex(&gid).map(|group| {
                let group = group.recover();
                (group.effective_option_snapshot(), group.runtime_options())
            });
            let stopped_result = if group_option_state.is_none() {
                group_man.find_stopped_result(&gid)
            } else {
                None
            };
            (group_option_state, stopped_result)
        } else {
            (None, None)
        };
        let group_exists = group_option_state.is_some();

        // Live groups own their option snapshot in core. This makes CLI,
        // session-restored, and RPC-created tasks follow the same rule.
        if let Some((Some(options), _)) = group_option_state.as_ref() {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serialize_request_options(options)?,
            ));
        }

        // C++ `GetOptionRpcMethod` reads `DownloadResult::option` once the
        // group is gone. A legacy result with no snapshot still exists, so it
        // must produce an empty object instead of a spurious unknown-GID
        // error.
        if let Some(result) = stopped_result {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serialize_request_options(&result.option_snapshot().cloned().unwrap_or_default())?,
            ));
        }

        // Preserve the legacy group-only adapter behavior for callers that
        // registered a RequestGroup without the RPC task snapshot.
        if let Some((_, runtime_options)) = group_option_state
            && !runtime_options.is_empty()
        {
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serialize_request_options(&runtime_options)?,
            ));
        }

        if group_exists {
            let global_opts = self.global_opts.read().await;
            return Ok(JsonRpcResponse::success(
                req.id.clone().unwrap_or_default(),
                serialize_request_options(&global_opts)?,
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
        self.handle_change_option_values(req.id.clone(), gid, raw_changes)
    }

    /// Owned network path for `aria2.changeOption`; move the options map out
    /// of the request instead of cloning it before normalization.
    pub(crate) async fn handle_change_option_owned(
        &self,
        req: &mut JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let gid: String = req.take_param(0)?;
        let raw_changes: HashMap<String, serde_json::Value> = req.take_param(1)?;
        self.handle_change_option_values(req.id.clone(), gid, raw_changes)
    }

    fn handle_change_option_values(
        &self,
        request_id: Option<serde_json::Value>,
        gid: String,
        raw_changes: HashMap<String, serde_json::Value>,
    ) -> Result<JsonRpcResponse, JsonRpcError> {
        let changes = normalize_rpc_options(&raw_changes);

        let group_man = self
            .group_man
            .as_ref()
            .ok_or_else(|| JsonRpcError::RpcExecution("RequestGroupMan is not wired".into()))?;
        let manager = group_man;
        manager
            .change_group_options(&gid, changes)
            .map_err(JsonRpcError::RpcExecution)?;

        Ok(JsonRpcResponse::success(
            request_id.unwrap_or_default(),
            "OK",
        ))
    }
}
