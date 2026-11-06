use std::collections::HashMap;

use aria2_core::config::{OptionRegistry, is_global_option_changeable};
use aria2_core::engine::engine_command::EngineCommand;
use aria2_rpc::{BackendError, BackendResponse, BackendResult};

use super::CoreRpcBackend;

impl CoreRpcBackend {
    pub(super) async fn change_global_option(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> Result<BackendResult, BackendError> {
        let changes: HashMap<_, _> = changes
            .into_iter()
            .filter(|(key, _)| is_global_option_changeable(key))
            .collect();
        let registry = OptionRegistry::new();
        let mut parsed = Vec::with_capacity(changes.len());
        for (key, value) in &changes {
            let value = registry
                .parse_rpc_value(key, value)
                .map_err(|error| Self::execution(format!("Option '{key}': {error}")))?;
            parsed.push((key.clone(), value));
        }

        // Complete all adapter-specific validation before touching the shared
        // configuration. This keeps a rejected request from partially
        // applying an unrelated option in the same RPC batch.
        let max_concurrent = parsed
            .iter()
            .find(|(key, _)| key == "max-concurrent-downloads")
            .map(|(_, value)| {
                let value = value.as_i64().ok_or_else(|| {
                    Self::execution("Option 'max-concurrent-downloads' must be an integer")
                })?;
                u32::try_from(value)
                    .map_err(|_| Self::execution("Option 'max-concurrent-downloads' is too large"))
            })
            .transpose()?;

        let runtime_rate_limits = if changes.contains_key("max-overall-download-limit")
            || changes.contains_key("max-overall-upload-limit")
        {
            let options = self.global_options().await;
            Some((
                parse_rate_limit(
                    changes
                        .get("max-overall-download-limit")
                        .or_else(|| options.get("max-overall-download-limit")),
                    "max-overall-download-limit",
                )?,
                parse_rate_limit(
                    changes
                        .get("max-overall-upload-limit")
                        .or_else(|| options.get("max-overall-upload-limit")),
                    "max-overall-upload-limit",
                )?,
            ))
        } else {
            None
        };

        #[cfg(feature = "bittorrent")]
        let tracker_sources = changes
            .get("bt-tracker-source")
            .map(|value| {
                let sources = rpc_value_to_string(value).ok_or_else(|| {
                    Self::execution("Option 'bt-tracker-source' must be a string or array")
                })?;
                if sources
                    .split([',', '\n'])
                    .map(str::trim)
                    .all(|source| source.is_empty())
                {
                    return Err(Self::execution(
                        "Option 'bt-tracker-source' must contain at least one source",
                    ));
                }
                Ok(sources)
            })
            .transpose()?;

        #[cfg(feature = "bittorrent")]
        let tracker_update_interval = changes
            .get("bt-tracker-update-interval")
            .map(|value| {
                let seconds = parse_u64(value, "bt-tracker-update-interval")?;
                if seconds == 0 {
                    return Err(Self::execution(
                        "Option 'bt-tracker-update-interval' must be greater than zero",
                    ));
                }
                Ok(seconds)
            })
            .transpose()?;

        #[cfg(feature = "bittorrent")]
        let public_trackers_enabled = parsed
            .iter()
            .find(|(key, _)| key == "enable-public-trackers")
            .map(|(_, value)| {
                value
                    .as_bool()
                    .ok_or_else(|| Self::execution("enable-public-trackers must be boolean"))
            })
            .transpose()?;

        {
            let mut config = self.config.write().await;
            for (key, value) in &parsed {
                config
                    .set_global_option(key, value.clone())
                    .await
                    .map_err(Self::execution)?;
            }
        }

        if let Some(max) = max_concurrent {
            self.send(EngineCommand::SetMaxConcurrent { max })?;
        }
        if let Some((download_limit, upload_limit)) = runtime_rate_limits {
            self.send(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            })?;
        }

        #[cfg(feature = "bittorrent")]
        {
            if let Some(sources) = tracker_sources {
                self.send(EngineCommand::SetPublicTrackerSources { sources })?;
            }
            if let Some(seconds) = tracker_update_interval {
                self.send(EngineCommand::SetPublicTrackerUpdateInterval { seconds })?;
            }
            if let Some(enabled) = public_trackers_enabled {
                self.send(EngineCommand::SetPublicTrackersEnabled { enabled })?;
            }
        }
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }
}

pub(super) fn normalize_options(
    options: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    options
        .iter()
        .filter_map(|(key, value)| {
            rpc_value_to_string(value).map(|value| (key.clone(), serde_json::Value::String(value)))
        })
        .collect()
}

fn rpc_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(rpc_value_to_string)
            .collect::<Option<Vec<_>>>()
            .map(|values| values.join("\n")),
        serde_json::Value::Null | serde_json::Value::Object(_) => None,
    }
}

fn parse_u64(value: &serde_json::Value, option: &str) -> Result<u64, BackendError> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .ok_or_else(|| BackendError::Execution(format!("Option '{option}' must be an integer")))
}

fn parse_rate_limit(
    value: Option<&serde_json::Value>,
    option: &str,
) -> Result<Option<u64>, BackendError> {
    let value =
        value.ok_or_else(|| BackendError::Execution(format!("Option '{option}' is missing")))?;
    let raw = rpc_value_to_string(value)
        .ok_or_else(|| BackendError::Execution(format!("Option '{option}' must be a byte rate")))?;
    let (number, multiplier) = match raw.chars().last() {
        Some('k' | 'K') => (&raw[..raw.len() - 1], 1024u64),
        Some('m' | 'M') => (&raw[..raw.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        Some('t' | 'T') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024 * 1024),
        _ => (raw.as_str(), 1),
    };
    let number = number
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| BackendError::Execution(format!("Option '{option}' must be a byte rate")))?;
    let bytes = number * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return Err(BackendError::Execution(format!(
            "Option '{option}' is too large"
        )));
    }
    Ok((bytes as u64 > 0).then_some(bytes as u64))
}
