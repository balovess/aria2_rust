//! Project changed global options onto commands for an already running engine.

use std::collections::{HashMap, HashSet};

use crate::config::OptionValue;

use super::engine_command::EngineCommand;

/// Build runtime commands for changed global options using the effective
/// post-update option snapshot.
///
/// Adapters remain responsible for parsing and committing their option
/// changes. This function keeps the mapping from shared engine options to
/// runtime commands consistent across those adapters.
pub fn runtime_commands_for_global_option_changes(
    changed_options: &HashSet<String>,
    effective_options: &HashMap<String, OptionValue>,
) -> Vec<EngineCommand> {
    let changed_options = changed_options
        .iter()
        .map(|name| crate::config::OptionRegistry::canonical_name(name))
        .collect::<HashSet<_>>();
    let mut commands = Vec::new();

    if changed_options.contains("max-concurrent-downloads")
        && let Some(max) = option_i64(effective_options, "max-concurrent-downloads")
        && let Ok(max) = u32::try_from(max)
    {
        commands.push(EngineCommand::SetMaxConcurrent { max });
    }

    if changed_options.contains("max-overall-download-limit")
        || changed_options.contains("max-overall-upload-limit")
    {
        commands.push(EngineCommand::SetGlobalRateLimit {
            download_limit: non_zero_limit(option_i64(
                effective_options,
                "max-overall-download-limit",
            )),
            upload_limit: non_zero_limit(option_i64(effective_options, "max-overall-upload-limit")),
        });
    }

    #[cfg(feature = "bittorrent")]
    {
        if changed_options.contains("bt-tracker-source")
            && let Some(sources) = option_list(effective_options, "bt-tracker-source")
        {
            commands.push(EngineCommand::SetPublicTrackerSources {
                sources: sources.join("\n"),
            });
        }
        if changed_options.contains("bt-tracker-update-interval")
            && let Some(seconds) = option_i64(effective_options, "bt-tracker-update-interval")
            && let Ok(seconds) = u64::try_from(seconds)
            && seconds > 0
        {
            commands.push(EngineCommand::SetPublicTrackerUpdateInterval { seconds });
        }
        if changed_options.contains("bt-tracker-source-timeout")
            && let Some(seconds) = option_i64(effective_options, "bt-tracker-source-timeout")
            && let Ok(seconds) = u64::try_from(seconds)
            && seconds > 0
        {
            commands.push(EngineCommand::SetPublicTrackerSourceTimeout { seconds });
        }
        if changed_options.contains("enable-public-trackers")
            && let Some(enabled) = option_bool(effective_options, "enable-public-trackers")
        {
            commands.push(EngineCommand::SetPublicTrackersEnabled { enabled });
        }
        if changed_options.contains("bt-incoming-peer-handshake-timeout")
            && let Some(seconds) =
                option_i64(effective_options, "bt-incoming-peer-handshake-timeout")
            && let Ok(seconds) = u64::try_from(seconds)
            && seconds > 0
        {
            commands.push(EngineCommand::SetIncomingPeerHandshakeTimeout { seconds });
        }
        if [
            "bt-utp-max-connections",
            "bt-utp-connect-timeout",
            "bt-utp-idle-timeout",
            "bt-utp-keepalive-interval",
        ]
        .iter()
        .any(|name| changed_options.contains(*name))
        {
            commands.push(EngineCommand::SetUtpSocketOptions {
                options: crate::config::utp_socket_options_from_values(
                    option_i64(effective_options, "bt-utp-max-connections"),
                    option_i64(effective_options, "bt-utp-connect-timeout"),
                    option_i64(effective_options, "bt-utp-idle-timeout"),
                    option_i64(effective_options, "bt-utp-keepalive-interval"),
                ),
            });
        }
    }

    commands
}

fn option_i64(options: &HashMap<String, OptionValue>, name: &str) -> Option<i64> {
    match options.get(name)? {
        OptionValue::Int(value) => Some(*value),
        OptionValue::Usize(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

#[cfg(feature = "bittorrent")]
fn option_bool(options: &HashMap<String, OptionValue>, name: &str) -> Option<bool> {
    match options.get(name)? {
        OptionValue::Bool(value) => Some(*value),
        _ => None,
    }
}

#[cfg(feature = "bittorrent")]
fn option_list(options: &HashMap<String, OptionValue>, name: &str) -> Option<Vec<String>> {
    match options.get(name)? {
        OptionValue::List(values) => Some(values.clone()),
        OptionValue::Str(value) => Some(
            value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

fn non_zero_limit(value: Option<i64>) -> Option<u64> {
    value
        .filter(|value| *value > 0)
        .and_then(|value| u64::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_changes_project_the_complete_effective_pair() {
        let changed = HashSet::from(["max-overall-download-limit".to_string()]);
        let effective = HashMap::from([
            (
                "max-overall-download-limit".to_string(),
                OptionValue::Int(2048),
            ),
            (
                "max-overall-upload-limit".to_string(),
                OptionValue::Int(1024),
            ),
        ]);

        let commands = runtime_commands_for_global_option_changes(&changed, &effective);
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            commands.first(),
            Some(EngineCommand::SetGlobalRateLimit {
                download_limit: Some(2048),
                upload_limit: Some(1024),
            })
        ));
    }

    #[test]
    fn unrepresentable_concurrency_is_not_wrapped_into_a_runtime_command() {
        let changed = HashSet::from(["max-concurrent-downloads".to_string()]);
        let effective = HashMap::from([(
            "max-concurrent-downloads".to_string(),
            OptionValue::Int(i64::from(u32::MAX) + 1),
        )]);

        assert!(runtime_commands_for_global_option_changes(&changed, &effective).is_empty());
    }

    #[test]
    fn compatibility_alias_projects_the_canonical_runtime_command() {
        let changed = HashSet::from(["max-downloads".to_string()]);
        let effective =
            HashMap::from([("max-concurrent-downloads".to_string(), OptionValue::Int(9))]);

        let commands = runtime_commands_for_global_option_changes(&changed, &effective);

        assert!(matches!(
            commands.as_slice(),
            [EngineCommand::SetMaxConcurrent { max: 9 }]
        ));
    }
}
