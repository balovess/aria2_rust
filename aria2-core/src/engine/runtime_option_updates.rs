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
        if changed_options.contains("bt-tracker-source-concurrency")
            && let Some(max_concurrent_sources) =
                option_i64(effective_options, "bt-tracker-source-concurrency")
                    .and_then(|value| usize::try_from(value).ok())
            && (1
                ..=aria2_protocol::bittorrent::tracker::public_list::MAX_TRACKER_SOURCE_CONCURRENCY)
                .contains(&max_concurrent_sources)
        {
            commands.push(EngineCommand::SetPublicTrackerSourceConcurrency {
                max_concurrent_sources,
            });
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
        if changed_options.contains("bt-max-open-files")
            && let Some(max) = option_i64(effective_options, "bt-max-open-files")
            && let Ok(max) = usize::try_from(max)
            && max > 0
        {
            commands.push(EngineCommand::SetBtMaxOpenFiles { max });
        }
        if (changed_options.contains("dht-max-peer-info-hashes")
            || changed_options.contains("dht-max-peers-per-info-hash"))
            && let (Some(max_info_hashes), Some(max_peers_per_info_hash)) = (
                option_i64(effective_options, "dht-max-peer-info-hashes")
                    .and_then(|value| usize::try_from(value).ok()),
                option_i64(effective_options, "dht-max-peers-per-info-hash")
                    .and_then(|value| usize::try_from(value).ok()),
            )
            && max_info_hashes > 0
            && max_peers_per_info_hash > 0
        {
            commands.push(EngineCommand::SetDhtPeerStorageConfig {
                config: aria2_protocol::bittorrent::dht::DhtPeerStorageConfig {
                    max_info_hashes,
                    max_peers_per_info_hash,
                },
            });
        }
        if changed_options.contains("dht-max-items")
            && let Some(max_items) = option_i64(effective_options, "dht-max-items")
                .and_then(|value| usize::try_from(value).ok())
            && max_items > 0
        {
            commands.push(EngineCommand::SetDhtItemStoreLimit { max_items });
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

    #[cfg(feature = "bittorrent")]
    #[test]
    fn bt_max_open_files_projects_to_a_process_runtime_command() {
        let changed = HashSet::from(["bt-max-open-files".to_string()]);
        let effective = HashMap::from([("bt-max-open-files".to_string(), OptionValue::Int(17))]);

        assert!(matches!(
            runtime_commands_for_global_option_changes(&changed, &effective).as_slice(),
            [EngineCommand::SetBtMaxOpenFiles { max: 17 }]
        ));
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn incoming_peer_handshake_timeout_projects_to_a_process_runtime_command() {
        let changed = HashSet::from(["bt-incoming-peer-handshake-timeout".to_string()]);
        let effective = HashMap::from([(
            "bt-incoming-peer-handshake-timeout".to_string(),
            OptionValue::Int(42),
        )]);

        assert!(matches!(
            runtime_commands_for_global_option_changes(&changed, &effective).as_slice(),
            [EngineCommand::SetIncomingPeerHandshakeTimeout { seconds: 42 }]
        ));
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn dht_storage_option_change_projects_the_complete_effective_config() {
        let changed = HashSet::from(["dht-max-peer-info-hashes".to_string()]);
        let effective = HashMap::from([
            (
                "dht-max-peer-info-hashes".to_string(),
                OptionValue::Int(128),
            ),
            (
                "dht-max-peers-per-info-hash".to_string(),
                OptionValue::Int(24),
            ),
        ]);

        assert!(matches!(
            runtime_commands_for_global_option_changes(&changed, &effective).as_slice(),
            [EngineCommand::SetDhtPeerStorageConfig { config }]
                if config.max_info_hashes == 128 && config.max_peers_per_info_hash == 24
        ));
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn dht_item_limit_projects_to_a_runtime_command() {
        let changed = HashSet::from(["dht-max-items".to_string()]);
        let effective = HashMap::from([("dht-max-items".to_string(), OptionValue::Int(512))]);

        assert!(matches!(
            runtime_commands_for_global_option_changes(&changed, &effective).as_slice(),
            [EngineCommand::SetDhtItemStoreLimit { max_items: 512 }]
        ));
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn public_tracker_catalog_options_project_to_process_commands() {
        let changed = HashSet::from([
            "bt-tracker-source".to_string(),
            "bt-tracker-update-interval".to_string(),
            "bt-tracker-source-concurrency".to_string(),
            "enable-public-trackers".to_string(),
        ]);
        let effective = HashMap::from([
            (
                "bt-tracker-source".to_string(),
                OptionValue::List(Vec::new()),
            ),
            (
                "bt-tracker-update-interval".to_string(),
                OptionValue::Int(604_800),
            ),
            (
                "bt-tracker-source-concurrency".to_string(),
                OptionValue::Int(2),
            ),
            (
                "enable-public-trackers".to_string(),
                OptionValue::Bool(false),
            ),
        ]);

        assert!(matches!(
            runtime_commands_for_global_option_changes(&changed, &effective).as_slice(),
            [
                EngineCommand::SetPublicTrackerSources { sources },
                EngineCommand::SetPublicTrackerUpdateInterval { seconds: 604_800 },
                EngineCommand::SetPublicTrackerSourceConcurrency {
                    max_concurrent_sources: 2,
                },
                EngineCommand::SetPublicTrackersEnabled { enabled: false },
            ] if sources.is_empty()
        ));
    }
}
