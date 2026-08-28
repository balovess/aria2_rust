//! Cross-entry-point contracts for the built-in configuration registry.
//!
//! These tests deliberately exercise the public configuration seams instead
//! of asserting the implementation of one adapter. A new option must remain
//! valid through the config-file parser, the generic CLI parser, and RPC
//! value validation at the same time.

use std::collections::BTreeSet;

use super::{ConfigParser, OptionDef, OptionRegistry, OptionType, OptionValue};

fn sample_value(definition: &OptionDef) -> String {
    match definition.opt_type() {
        OptionType::String | OptionType::Path => "contract-value".to_string(),
        OptionType::Ipv4Address => "127.0.0.1".to_string(),
        OptionType::Boolean => "true".to_string(),
        OptionType::Integer => {
            let lower = definition.min.unwrap_or(1).max(0) as u64;
            let value = definition.max.map_or(lower.max(1), |max| lower.min(max));
            value.to_string()
        }
        OptionType::IntegerRange => {
            let lower = definition.min.unwrap_or(1).max(0);
            lower.to_string()
        }
        OptionType::Float => definition
            .default_value()
            .as_f64()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "1.5".to_string()),
        OptionType::List => "first,second".to_string(),
        OptionType::Enum => definition
            .allowed_values()
            .first()
            .copied()
            .or_else(|| definition.default_value().as_str())
            .unwrap_or("contract-value")
            .to_string(),
        OptionType::IndexOut => "1=contract-output.bin".to_string(),
        OptionType::PiecePriority => "head=1K".to_string(),
        OptionType::Size => {
            let lower = definition.min.unwrap_or(1).max(0) as u64;
            let value = definition.max.map_or(lower.max(1), |max| lower.min(max));
            value.to_string()
        }
    }
}

fn non_default_sample(definition: &OptionDef) -> String {
    let candidates = match definition.opt_type() {
        OptionType::Boolean => {
            vec![(!definition.default_value().as_bool().unwrap_or(false)).to_string()]
        }
        OptionType::String | OptionType::Path => {
            if definition.name() == "checksum" {
                vec!["sha-256=contract-digest".to_string()]
            } else {
                vec!["contract-consumer-value".to_string()]
            }
        }
        OptionType::Ipv4Address => vec!["192.0.2.1".to_string(), "127.0.0.1".to_string()],
        OptionType::Integer | OptionType::IntegerRange | OptionType::Size => {
            ["1", "2", "7", "1024", "2048", "65535", "1048576"]
                .into_iter()
                .map(str::to_string)
                .collect()
        }
        OptionType::Float => vec!["1.5".to_string(), "2.5".to_string(), "7.5".to_string()],
        OptionType::List => vec!["first,second".to_string()],
        OptionType::Enum => definition
            .allowed_values()
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        OptionType::IndexOut => vec!["1=contract-output.bin".to_string()],
        OptionType::PiecePriority => vec!["head=1K".to_string()],
    };

    candidates
        .into_iter()
        .find(|candidate| {
            definition
                .parse_value(candidate)
                .is_ok_and(|parsed| &parsed != definition.default_value())
        })
        .or_else(|| {
            candidates_from_default(definition)
                .into_iter()
                .find(|candidate| definition.parse_value(candidate).is_ok())
        })
        .unwrap_or_else(|| sample_value(definition))
}

fn candidates_from_default(definition: &OptionDef) -> Vec<String> {
    match definition.default_value() {
        OptionValue::Bool(value) => vec![value.to_string()],
        OptionValue::Int(value) => vec![value.to_string()],
        OptionValue::Usize(value) => vec![value.to_string()],
        OptionValue::Float(value) => vec![value.to_string()],
        OptionValue::Str(value) => vec![value.clone()],
        OptionValue::List(values) => vec![values.join(",")],
        OptionValue::None => Vec::new(),
    }
}

fn session_wire_name(name: &str) -> &str {
    match name {
        "max-tries" => "max-retries",
        "bt-force-encryption" => "bt-force-encrypt",
        _ => name,
    }
}

fn rpc_sample(definition: &OptionDef, raw: &str) -> serde_json::Value {
    match definition.opt_type() {
        OptionType::Boolean => serde_json::json!(true),
        OptionType::Integer | OptionType::IntegerRange | OptionType::Size => raw
            .parse::<i64>()
            .map_or_else(|_| serde_json::json!(raw), |value| serde_json::json!(value)),
        OptionType::Float => raw
            .parse::<f64>()
            .map_or_else(|_| serde_json::json!(raw), |value| serde_json::json!(value)),
        OptionType::List if definition.cumulative_delimiter.is_some() => {
            serde_json::json!([raw])
        }
        _ => serde_json::json!(raw),
    }
}

fn assert_unique(label: &str, values: &[&str]) {
    let unique = values.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        values.len(),
        "{} contains duplicate option names: {:?}",
        label,
        values
            .iter()
            .copied()
            .filter(|value| values
                .iter()
                .filter(|candidate| candidate == &value)
                .count()
                > 1)
            .collect::<Vec<_>>()
    );
}

#[test]
fn every_registered_option_uses_one_parser_contract() {
    let registry = OptionRegistry::new();
    let mut config_content = String::new();
    let mut cli_args = Vec::new();
    let mut rpc_options = std::collections::HashMap::new();

    for definition in registry.all().values() {
        if !definition.is_supported() {
            assert!(
                definition.parse_default_value().is_none(),
                "unsupported option '{}' must not inject a default",
                definition.name()
            );
            assert!(
                definition.parse_value("1").is_err(),
                "unsupported option '{}' must reject explicit values",
                definition.name()
            );
            continue;
        }

        let raw = sample_value(definition);
        assert!(
            definition.parse_value(&raw).is_ok(),
            "sample value for '{}' must be valid: {:?}",
            definition.name(),
            definition.parse_value(&raw).err()
        );
        config_content.push_str(definition.name());
        config_content.push('=');
        config_content.push_str(&raw);
        config_content.push('\n');

        let cli_value = if definition.opt_type() == OptionType::Boolean {
            "true".to_string()
        } else {
            raw.clone()
        };
        cli_args.push(format!("--{}={}", definition.name(), cli_value));
        rpc_options.insert(definition.name().to_string(), rpc_sample(definition, &raw));
    }

    let temp_dir = tempfile::tempdir().expect("contract test temp directory");
    let config_path = temp_dir.path().join("all-options.conf");
    std::fs::write(&config_path, config_content).expect("write all-options config fixture");

    let mut file_parser = ConfigParser::new();
    file_parser.parse_file(config_path.to_str().expect("UTF-8 temp path"));
    assert!(
        !file_parser.has_errors(),
        "config-file errors: {:?}",
        file_parser.errors()
    );
    let cli_refs = cli_args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&cli_refs);
    assert!(
        !cli_parser.has_errors(),
        "CLI errors: {:?}",
        cli_parser.errors()
    );
    let supported_count = registry
        .all()
        .values()
        .filter(|definition| definition.is_supported())
        .count();
    assert_eq!(file_parser.options().len(), supported_count);
    assert_eq!(cli_parser.options().len(), supported_count);

    for (name, value) in rpc_options {
        assert!(
            registry.parse_rpc_value(&name, &value).is_ok(),
            "RPC sample for '{}' must use the same registry parser",
            name
        );
    }
}

#[tokio::test]
async fn unsupported_options_are_ignored_in_config_files_but_rejected_elsewhere() {
    let registry = OptionRegistry::new();
    let unsupported = registry
        .all()
        .values()
        .filter(|definition| !definition.is_supported())
        .map(|definition| definition.name().to_owned())
        .collect::<Vec<_>>();
    assert!(
        !unsupported.is_empty(),
        "the unsupported-option contract must exercise at least one compatibility key"
    );

    let config_content = unsupported
        .iter()
        .map(|name| format!("{name}=unsupported-contract-value\n"))
        .collect::<String>();
    let temp_dir = tempfile::tempdir().expect("unsupported-option contract directory");
    let config_path = temp_dir.path().join("unsupported-options.conf");
    std::fs::write(&config_path, config_content).expect("write unsupported config fixture");

    let mut file_parser = ConfigParser::new();
    file_parser.parse_file(config_path.to_str().expect("UTF-8 temp path"));
    assert!(
        !file_parser.has_errors(),
        "legacy config options should be ignored: {:?}",
        file_parser.errors()
    );
    assert!(file_parser.options().is_empty());

    let cli_args = unsupported
        .iter()
        .map(|name| format!("--{name}=unsupported-contract-value"))
        .collect::<Vec<_>>();
    let cli_refs = cli_args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&cli_refs);
    assert_eq!(cli_parser.errors().len(), unsupported.len());
    assert!(cli_parser.options().is_empty());

    let mut manager = super::ConfigManager::new();
    for name in &unsupported {
        assert!(
            registry
                .parse_rpc_value(name, &serde_json::json!("unsupported-contract-value"))
                .is_err(),
            "RPC registry input for unsupported option '{}' must be rejected",
            name
        );
        assert!(
            crate::request::request_group::DownloadOptions::try_from_rpc_options(
                &std::collections::HashMap::from([(
                    name.clone(),
                    serde_json::json!("unsupported-contract-value"),
                )])
            )
            .is_err(),
            "unsupported option '{}' must not enter DownloadOptions",
            name
        );
        assert!(
            manager
                .set_global_option(
                    name,
                    OptionValue::Str("unsupported-contract-value".to_string())
                )
                .await
                .is_err(),
            "ConfigManager must reject unsupported option '{}'",
            name
        );
        assert_eq!(
            manager.get_global_option(name).await,
            None,
            "unsupported option '{}' must not enter ConfigManager state",
            name
        );
    }
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn compatibility_aliases_use_one_canonical_storage_key() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[
        "--enable-lpd=true",
        "--dht-message-path=legacy-dht.dat",
        "--max-retries=7",
    ]);
    assert!(
        !parser.has_errors(),
        "alias parse errors: {:?}",
        parser.errors()
    );
    assert_eq!(parser.get_bool("bt-enable-lpd"), Some(true));
    assert_eq!(parser.get_bool("enable-lpd"), Some(true));
    assert_eq!(parser.get_str("dht-file-path"), Some("legacy-dht.dat"));
    assert_eq!(parser.get_i64("max-tries"), Some(7));
    assert!(!parser.options().contains_key("enable-lpd"));
    assert!(!parser.options().contains_key("dht-message-path"));
    assert!(!parser.options().contains_key("max-retries"));

    let mut manager = super::ConfigManager::new();
    manager
        .set_global_option("enable-lpd", OptionValue::Bool(true))
        .await
        .expect("LPD alias must validate");
    manager
        .set_global_option(
            "dht-message-path",
            OptionValue::Str("manager-dht.dat".into()),
        )
        .await
        .expect("DHT path alias must validate");
    manager
        .set_global_option("max-retries", OptionValue::Int(9))
        .await
        .expect("retry alias must validate");

    assert_eq!(manager.get_global_bool("bt-enable-lpd").await, Some(true));
    assert_eq!(
        manager.get_global_str("dht-file-path").await.as_deref(),
        Some("manager-dht.dat")
    );
    assert_eq!(manager.get_global_i64("max-tries").await, Some(9));
    let all = manager.get_all_global_options().await;
    assert!(!all.contains_key("enable-lpd"));
    assert!(!all.contains_key("dht-message-path"));
    assert!(!all.contains_key("max-retries"));
}

#[test]
fn runtime_policy_names_are_unique_and_registered() {
    use super::runtime::{
        INITIAL_IDENTITY_OPTIONS, INITIAL_REQUEST_OPTIONS, INITIAL_SNAPSHOT_WIRE_OPTIONS,
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
        RUNTIME_GLOBAL_CHANGEABLE_OPTIONS,
    };

    let registry = OptionRegistry::new();
    assert_unique("global runtime policy", RUNTIME_GLOBAL_CHANGEABLE_OPTIONS);
    assert_unique("initial request policy", INITIAL_REQUEST_OPTIONS);
    assert_unique("immediate task policy", RUNTIME_CHANGEABLE_OPTIONS);
    assert_unique(
        "reserved task policy",
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
    );
    assert_unique("initial identity policy", INITIAL_IDENTITY_OPTIONS);
    assert_unique(
        "initial snapshot wire policy",
        INITIAL_SNAPSHOT_WIRE_OPTIONS,
    );

    for name in INITIAL_REQUEST_OPTIONS
        .iter()
        .chain(RUNTIME_GLOBAL_CHANGEABLE_OPTIONS)
        .chain(RUNTIME_CHANGEABLE_OPTIONS)
        .chain(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS)
    {
        #[cfg(feature = "bittorrent")]
        let feature_gated = false;
        #[cfg(not(feature = "bittorrent"))]
        let feature_gated = name.starts_with("bt-")
            || matches!(
                *name,
                "enable-peer-exchange"
                    | "follow-torrent"
                    | "index-out"
                    | "max-overall-upload-limit"
                    | "max-upload-limit"
                    | "seed-ratio"
                    | "seed-time"
                    | "select-file"
            );

        if !feature_gated {
            assert!(
                registry.contains(name),
                "runtime policy '{}' has no registered option in this build",
                name
            );
        }
    }

    for name in INITIAL_IDENTITY_OPTIONS {
        assert!(
            INITIAL_REQUEST_OPTIONS.contains(name),
            "identity consumer '{}' is not an initial request option",
            name
        );
    }

    for name in INITIAL_SNAPSHOT_WIRE_OPTIONS {
        assert!(
            INITIAL_REQUEST_OPTIONS.contains(name),
            "snapshot wire option '{}' is not an initial request option",
            name
        );
    }
}

#[test]
fn every_initial_option_reaches_download_options_or_an_explicit_snapshot_consumer() {
    use super::runtime::{INITIAL_IDENTITY_OPTIONS, INITIAL_REQUEST_OPTIONS};

    let registry = OptionRegistry::new();
    let mut config_content = String::new();
    let mut cli_args = Vec::new();
    for name in INITIAL_REQUEST_OPTIONS {
        let Some(definition) = registry.get(name) else {
            continue;
        };
        let raw = non_default_sample(definition);
        config_content.push_str(name);
        config_content.push('=');
        config_content.push_str(&raw);
        config_content.push('\n');
        cli_args.push(format!("--{}={}", name, raw));
    }

    let temp_dir = tempfile::tempdir().expect("initial-option contract directory");
    let config_path = temp_dir.path().join("initial-options.conf");
    std::fs::write(&config_path, config_content).expect("write initial-option config fixture");

    let mut file_parser = ConfigParser::new();
    file_parser.parse_file(config_path.to_str().expect("UTF-8 temp path"));
    assert!(
        !file_parser.has_errors(),
        "initial-option config errors: {:?}",
        file_parser.errors()
    );
    let file_download_options =
        crate::request::request_group::DownloadOptions::from_option_values(file_parser.options());
    let file_session =
        crate::session::session_entry::download_options_to_map(&file_download_options);

    let cli_refs = cli_args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&cli_refs);
    assert!(
        !cli_parser.has_errors(),
        "initial-option CLI errors: {:?}",
        cli_parser.errors()
    );
    let cli_download_options =
        crate::request::request_group::DownloadOptions::from_option_values(cli_parser.options());
    let cli_session = crate::session::session_entry::download_options_to_map(&cli_download_options);

    let mut missing: Vec<&str> = Vec::new();
    for name in INITIAL_REQUEST_OPTIONS {
        let Some(definition) = registry.get(name) else {
            continue;
        };
        if INITIAL_IDENTITY_OPTIONS.contains(name) {
            continue;
        }
        let raw = non_default_sample(definition);
        let options = std::collections::HashMap::from([((*name).to_string(), raw.clone())]);
        let download_options =
            crate::request::request_group::DownloadOptions::from_option_strings(&options);
        let rpc_options = std::collections::HashMap::from([(
            (*name).to_string(),
            serde_json::Value::String(raw.clone()),
        )]);
        let rpc_download_options =
            crate::request::request_group::DownloadOptions::try_from_rpc_options(&rpc_options)
                .unwrap_or_else(|error| {
                    panic!(
                        "RPC option '{}' must reach DownloadOptions: {}",
                        name, error
                    )
                });
        let rpc_session =
            crate::session::session_entry::download_options_to_map(&rpc_download_options);
        let snapshot = crate::config::project_initial_options(
            options
                .iter()
                .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone()))),
        );
        let serialized = crate::session::session_entry::download_options_to_map_with_snapshot(
            &download_options,
            Some(&snapshot),
        );
        let wire_name = session_wire_name(name);
        let reaches_all_entry_points = [
            ("config-file", &file_session),
            ("cli", &cli_session),
            ("rpc", &rpc_session),
            ("session-snapshot", &serialized),
        ]
        .into_iter()
        .all(|(source, map)| {
            if map.get(wire_name) != Some(&raw) {
                eprintln!(
                    "initial option '{}' from {} serialized as {:?}, expected {:?}",
                    name,
                    source,
                    map.get(wire_name),
                    raw
                );
                false
            } else {
                true
            }
        });
        if !reaches_all_entry_points {
            missing.push(name);
        }
    }

    assert!(
        missing.is_empty(),
        "initial options have no DownloadOptions/session consumer: {:?}; add a typed field mapping",
        missing
    );
}

#[test]
fn every_task_runtime_policy_has_a_real_download_option_consumer() {
    use super::runtime::{RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS};

    let registry = OptionRegistry::new();
    let mut group = crate::request::request_group::RequestGroup::new(
        crate::request::request_group::GroupId::new(42),
        vec!["http://example.test/file".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    );
    let mut seen = BTreeSet::new();

    for name in RUNTIME_CHANGEABLE_OPTIONS
        .iter()
        .chain(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS)
    {
        if !seen.insert(*name) {
            continue;
        }
        let Some(definition) = registry.get(name) else {
            // Feature-specific policies are absent from a build without the
            // corresponding protocol feature.
            continue;
        };
        let raw = non_default_sample(definition);
        let value = serde_json::Value::String(raw);
        assert!(
            crate::request::request_group::RequestGroup::validate_option_update(name, &value)
                .expect("runtime option validation must not fail"),
            "runtime policy '{}' has no typed DownloadOptions consumer",
            name
        );
        assert!(
            group
                .try_update_option(name, value)
                .expect("runtime option update must not fail"),
            "runtime policy '{}' was not applied to the live request group",
            name
        );
    }
}

#[tokio::test]
async fn disk_cache_contract_reaches_session_and_real_writer_io() {
    use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
    use crate::request::request_group::DownloadOptions;
    use crate::session::session_entry::download_options_to_map;

    let temp_dir = tempfile::tempdir().expect("disk-cache contract directory");
    let config_path = temp_dir.path().join("disk-cache.conf");
    std::fs::write(&config_path, "disk-cache=0\n").expect("write disk-cache config fixture");

    let mut file_parser = ConfigParser::new();
    file_parser.parse_file(config_path.to_str().expect("UTF-8 temp path"));
    assert!(
        !file_parser.has_errors(),
        "config errors: {:?}",
        file_parser.errors()
    );
    let file_options = DownloadOptions::from_option_values(file_parser.options());

    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&["--disk-cache=0"]);
    assert!(
        !cli_parser.has_errors(),
        "CLI errors: {:?}",
        cli_parser.errors()
    );
    let cli_options = DownloadOptions::from_option_values(cli_parser.options());

    let rpc_options = DownloadOptions::try_from_rpc_options(&std::collections::HashMap::from([(
        "disk-cache".to_string(),
        serde_json::json!(0),
    )]))
    .expect("RPC disk-cache value must use the shared parser");

    for (source, options) in [
        ("config-file", &file_options),
        ("CLI", &cli_options),
        ("RPC", &rpc_options),
    ] {
        assert_eq!(
            options.disk_cache,
            Some(0),
            "{} must preserve an explicit zero disk-cache value",
            source
        );
        assert_eq!(
            options.disk_cache_size_bytes(),
            None,
            "{} zero disk-cache must disable the write-back cache",
            source
        );
    }

    let session = download_options_to_map(&rpc_options);
    assert_eq!(session.get("disk-cache").map(String::as_str), Some("0"));
    let restored = DownloadOptions::from_option_strings(&session);
    assert_eq!(restored.disk_cache, Some(0));

    let direct_path = temp_dir.path().join("direct.bin");
    let mut direct_writer = CachedDiskWriter::new_with_mmap_bytes(
        &direct_path,
        Some(64),
        file_options.disk_cache_size_bytes(),
        false,
    );
    direct_writer.open().await.unwrap();
    direct_writer.write_at(0, b"direct").await.unwrap();
    assert_eq!(
        &tokio::fs::read(&direct_path).await.unwrap()[..6],
        b"direct",
        "disk-cache=0 must make a small write visible before flush"
    );
    direct_writer.close().await.unwrap();

    let cached_path = temp_dir.path().join("cached.bin");
    let mut cached_writer =
        CachedDiskWriter::new_with_mmap_bytes(&cached_path, Some(64), Some(4096), false);
    cached_writer.open().await.unwrap();
    cached_writer.write_at(0, b"cached").await.unwrap();
    assert_eq!(
        &tokio::fs::read(&cached_path).await.unwrap()[..6],
        &[0; 6],
        "a non-zero disk-cache must buffer small writes until flush"
    );
    cached_writer.flush().await.unwrap();
    assert_eq!(
        &tokio::fs::read(&cached_path).await.unwrap()[..6],
        b"cached",
        "cached bytes must reach the file at the writer flush boundary"
    );
    cached_writer.close().await.unwrap();
}

#[test]
fn every_registered_option_has_one_explicit_production_owner() {
    let registry = OptionRegistry::new();
    let mut owners = std::collections::HashMap::<super::OptionOwner, Vec<String>>::new();

    for (name, definition) in registry.all() {
        let expected = OptionRegistry::owner_for_name(name).unwrap_or_else(|| {
            panic!(
                "registered option '{}' has no canonical production owner mapping",
                name
            )
        });
        assert_eq!(definition.owner(), expected);
        owners
            .entry(definition.owner())
            .or_default()
            .push(name.clone());
    }

    assert!(
        !owners.is_empty(),
        "the registry must contain owned options"
    );
    assert!(
        owners
            .values()
            .flatten()
            .all(|name| OptionRegistry::canonical_name(name) == name),
        "registry must contain canonical names only: {:?}",
        owners
    );
}

#[tokio::test]
async fn every_global_runtime_policy_reaches_config_manager_storage() {
    use super::runtime::RUNTIME_GLOBAL_CHANGEABLE_OPTIONS;

    let registry = OptionRegistry::new();
    let mut manager = super::ConfigManager::new();
    for name in RUNTIME_GLOBAL_CHANGEABLE_OPTIONS {
        let Some(definition) = registry.get(name) else {
            continue;
        };
        let raw = non_default_sample(definition);
        let parsed = definition.parse_value(&raw).unwrap_or_else(|error| {
            panic!("global option '{}' sample is invalid: {}", name, error)
        });
        manager
            .set_global_option(name, parsed.clone())
            .await
            .unwrap_or_else(|error| panic!("global option '{}' was not stored: {}", name, error));
        assert_eq!(
            manager.get_global_option(name).await,
            Some(parsed),
            "global option '{}' did not round-trip through ConfigManager",
            name
        );
    }
}

#[cfg(feature = "bittorrent")]
#[test]
fn bittorrent_options_share_config_cli_rpc_and_session_contract() {
    use crate::request::request_group::DownloadOptions;
    use crate::session::session_entry::download_options_to_map;

    let config_content = "bt-exclude-tracker=http://excluded.test/announce,udp://excluded.test:6969\n\
bt-tracker=http://custom-one.test/announce,udp://custom-two.test:6969\n\
bt-external-ip=203.0.113.7\n\
bt-tracker-interval=17\n\
bt-tracker-timeout=23\n\
bt-tracker-connect-timeout=11\n\
bt-request-peer-speed-limit=128K\n\
enable-peer-exchange=false\n\
bt-load-saved-metadata=true\n\
bt-save-metadata=true\n\
bt-metadata-only=true\n";
    let temp_dir = tempfile::tempdir().expect("BitTorrent config contract directory");
    let config_path = temp_dir.path().join("bittorrent-options.conf");
    std::fs::write(&config_path, config_content).expect("write BitTorrent config fixture");

    let mut file_parser = ConfigParser::new();
    file_parser.parse_file(config_path.to_str().expect("UTF-8 temp path"));
    assert!(
        !file_parser.has_errors(),
        "config errors: {:?}",
        file_parser.errors()
    );
    let file_options = DownloadOptions::from_option_values(file_parser.options());

    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&[
        "--bt-exclude-tracker=http://excluded.test/announce",
        "--bt-exclude-tracker=udp://excluded.test:6969",
        "--bt-tracker=http://custom-one.test/announce,udp://custom-two.test:6969",
        "--bt-external-ip=203.0.113.7",
        "--bt-tracker-interval=17",
        "--bt-tracker-timeout=23",
        "--bt-tracker-connect-timeout=11",
        "--bt-request-peer-speed-limit=128K",
        "--enable-peer-exchange=false",
        "--bt-load-saved-metadata=true",
        "--bt-save-metadata=true",
        "--bt-metadata-only=true",
    ]);
    assert!(
        !cli_parser.has_errors(),
        "CLI errors: {:?}",
        cli_parser.errors()
    );
    let cli_options = DownloadOptions::from_option_values(cli_parser.options());

    let rpc_values = std::collections::HashMap::from([
        (
            "bt-exclude-tracker".to_string(),
            serde_json::json!(["http://excluded.test/announce", "udp://excluded.test:6969"]),
        ),
        (
            "bt-tracker".to_string(),
            serde_json::json!("http://custom-one.test/announce,udp://custom-two.test:6969"),
        ),
        (
            "bt-external-ip".to_string(),
            serde_json::json!("203.0.113.7"),
        ),
        ("bt-tracker-interval".to_string(), serde_json::json!(17)),
        ("bt-tracker-timeout".to_string(), serde_json::json!(23)),
        (
            "bt-tracker-connect-timeout".to_string(),
            serde_json::json!(11),
        ),
        (
            "bt-request-peer-speed-limit".to_string(),
            serde_json::json!("128K"),
        ),
        ("enable-peer-exchange".to_string(), serde_json::json!(false)),
        (
            "bt-load-saved-metadata".to_string(),
            serde_json::json!(true),
        ),
        ("bt-save-metadata".to_string(), serde_json::json!(true)),
        ("bt-metadata-only".to_string(), serde_json::json!(true)),
    ]);
    let rpc_options = DownloadOptions::try_from_rpc_options(&rpc_values)
        .expect("RPC BitTorrent options must use the shared typed parser");

    for (source, options) in [
        ("config-file", &file_options),
        ("CLI", &cli_options),
        ("RPC", &rpc_options),
    ] {
        assert_eq!(
            options.bt_exclude_tracker.as_deref(),
            Some(
                [
                    "http://excluded.test/announce".to_string(),
                    "udp://excluded.test:6969".to_string(),
                ]
                .as_slice()
            ),
            "{} bt-exclude-tracker",
            source
        );
        assert_eq!(
            options.bt_tracker.as_deref(),
            Some(
                [
                    "http://custom-one.test/announce".to_string(),
                    "udp://custom-two.test:6969".to_string(),
                ]
                .as_slice()
            ),
            "{} bt-tracker",
            source
        );
        assert_eq!(options.bt_external_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(options.bt_tracker_interval, 17);
        assert_eq!(options.bt_tracker_timeout, 23);
        assert_eq!(options.bt_tracker_connect_timeout, 11);
        assert_eq!(options.bt_request_peer_speed_limit, 128 * 1024);
        assert!(!options.enable_peer_exchange);
        assert!(options.bt_load_saved_metadata);
        assert!(options.bt_save_metadata);
        assert!(options.bt_metadata_only);
    }

    let session = download_options_to_map(&rpc_options);
    for (name, expected) in [
        (
            "bt-exclude-tracker",
            "http://excluded.test/announce,udp://excluded.test:6969",
        ),
        (
            "bt-tracker",
            "http://custom-one.test/announce,udp://custom-two.test:6969",
        ),
        ("bt-external-ip", "203.0.113.7"),
        ("bt-tracker-interval", "17"),
        ("bt-tracker-timeout", "23"),
        ("bt-tracker-connect-timeout", "11"),
        ("bt-request-peer-speed-limit", "131072"),
        ("enable-peer-exchange", "false"),
        ("bt-load-saved-metadata", "true"),
        ("bt-save-metadata", "true"),
        ("bt-metadata-only", "true"),
    ] {
        assert_eq!(
            session.get(name).map(String::as_str),
            Some(expected),
            "session {}",
            name
        );
    }

    let restored = DownloadOptions::from_option_strings(&session);
    assert_eq!(restored.bt_tracker, rpc_options.bt_tracker);
    assert_eq!(restored.bt_exclude_tracker, rpc_options.bt_exclude_tracker);
    assert_eq!(restored.bt_external_ip, rpc_options.bt_external_ip);
    assert_eq!(
        restored.bt_tracker_interval,
        rpc_options.bt_tracker_interval
    );
    assert_eq!(restored.bt_tracker_timeout, rpc_options.bt_tracker_timeout);
    assert_eq!(
        restored.bt_tracker_connect_timeout,
        rpc_options.bt_tracker_connect_timeout
    );
    assert_eq!(
        restored.bt_request_peer_speed_limit,
        rpc_options.bt_request_peer_speed_limit
    );
    assert_eq!(
        restored.enable_peer_exchange,
        rpc_options.enable_peer_exchange
    );
    assert_eq!(
        restored.bt_load_saved_metadata,
        rpc_options.bt_load_saved_metadata
    );
    assert_eq!(restored.bt_save_metadata, rpc_options.bt_save_metadata);
    assert_eq!(restored.bt_metadata_only, rpc_options.bt_metadata_only);
}

#[cfg(feature = "bittorrent")]
#[test]
fn bittorrent_execution_options_round_trip_through_task_session() {
    use crate::request::request_group::DownloadOptions;
    use crate::session::session_entry::download_options_to_map;

    let cases = [
        ("bt-enable-web-seed", "false"),
        ("bt-max-open-files", "17"),
        ("bt-peer-blocklist", "blocked-peers.txt"),
        ("bt-keep-alive-interval", "31"),
        ("bt-timeout", "181"),
        ("bt-request-timeout", "61"),
        ("peer-connection-timeout", "16"),
        ("peer-id-prefix", "AZ1234"),
        ("peer-agent", "contract-peer-agent/1"),
        ("dht-message-timeout", "11"),
        ("enable-dht6", "true"),
        ("dht-listen-addr6", "::1"),
        ("dht-entry-point-host", "bootstrap.example"),
        ("dht-entry-point-port", "6881"),
        ("dht-entry-point6", "[2001:db8::1]:6881"),
        ("dht-entry-point-host6", "bootstrap6.example"),
        ("dht-entry-point-port6", "6882"),
        ("dht-file-path6", "dht6.dat"),
        ("dht-listen-addr", "127.0.0.1"),
    ];

    let raw = cases
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect::<std::collections::HashMap<_, _>>();
    let options = DownloadOptions::from_option_strings(&raw);
    let serialized = download_options_to_map(&options);

    for (name, expected) in cases {
        assert_eq!(
            serialized.get(name).map(String::as_str),
            Some(expected),
            "BitTorrent execution option '{}' must have a typed session consumer",
            name
        );
    }
}

#[test]
fn initial_option_snapshot_is_reserved_for_wire_fidelity_only() {
    assert_eq!(
        super::runtime::INITIAL_SNAPSHOT_WIRE_OPTIONS,
        &["min-split-size"],
        "raw snapshot preservation must not become an execution fallback"
    );
}

#[test]
fn registered_defaults_are_explicitly_reparsable() {
    let registry = OptionRegistry::new();
    for definition in registry.all().values() {
        if !matches!(definition.default_value(), OptionValue::None) {
            let raw = definition.default_value().to_string();
            if definition.is_supported() {
                assert!(
                    definition.parse_value(&raw).is_ok(),
                    "default for '{}' is not accepted by its own definition",
                    definition.name()
                );
            } else {
                assert!(
                    definition.parse_default_value().is_none(),
                    "unsupported option '{}' must not expose a runtime default",
                    definition.name()
                );
            }
        }
    }
}
