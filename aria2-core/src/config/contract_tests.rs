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
    assert_eq!(file_parser.options().len(), registry.count());

    let cli_refs = cli_args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut cli_parser = ConfigParser::new();
    cli_parser.parse_cli_args(&cli_refs);
    assert!(
        !cli_parser.has_errors(),
        "CLI errors: {:?}",
        cli_parser.errors()
    );
    assert_eq!(cli_parser.options().len(), registry.count());

    for (name, value) in rpc_options {
        assert!(
            registry.parse_rpc_value(&name, &value).is_ok(),
            "RPC sample for '{}' must use the same registry parser",
            name
        );
    }
}

#[test]
fn runtime_policy_names_are_unique_and_registered() {
    use super::runtime::{
        INITIAL_REQUEST_OPTIONS, INITIAL_SNAPSHOT_CONSUMER_OPTIONS,
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
        RUNTIME_GLOBAL_CHANGEABLE_OPTIONS, is_snapshot_consumer,
    };

    let registry = OptionRegistry::new();
    assert_unique("global runtime policy", RUNTIME_GLOBAL_CHANGEABLE_OPTIONS);
    assert_unique("initial request policy", INITIAL_REQUEST_OPTIONS);
    assert_unique("immediate task policy", RUNTIME_CHANGEABLE_OPTIONS);
    assert_unique(
        "reserved task policy",
        RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS,
    );
    assert_unique(
        "initial snapshot consumer policy",
        INITIAL_SNAPSHOT_CONSUMER_OPTIONS,
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

    for name in INITIAL_SNAPSHOT_CONSUMER_OPTIONS {
        assert!(
            INITIAL_REQUEST_OPTIONS.contains(name),
            "snapshot consumer '{}' is not an initial request option",
            name
        );
        assert!(
            is_snapshot_consumer(name),
            "snapshot consumer '{}' must be recognized by the shared policy",
            name
        );
    }
}

#[test]
fn every_initial_option_reaches_download_options_or_an_explicit_snapshot_consumer() {
    use super::runtime::{INITIAL_REQUEST_OPTIONS, is_snapshot_consumer};

    let registry = OptionRegistry::new();
    let mut missing = Vec::new();
    for name in INITIAL_REQUEST_OPTIONS {
        let Some(definition) = registry.get(name) else {
            continue;
        };
        let raw = non_default_sample(definition);
        let options = std::collections::HashMap::from([((*name).to_string(), raw)]);
        let download_options =
            crate::request::request_group::DownloadOptions::from_option_strings(&options);
        let snapshot = crate::config::project_initial_options(
            options
                .iter()
                .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone()))),
        );
        let serialized = crate::session::session_entry::download_options_to_map_with_snapshot(
            &download_options,
            Some(&snapshot),
        );
        if !serialized.contains_key(session_wire_name(name)) {
            assert!(
                is_snapshot_consumer(name),
                "initial option '{}' must either map to DownloadOptions/session or be an explicit snapshot consumer",
                name
            );
            missing.push(*name);
        }
    }

    assert!(
        missing.is_empty(),
        "initial options have no DownloadOptions/session consumer: {:?}; add a real field mapping or explicitly move the option to a snapshot consumer",
        missing
    );
}

#[test]
fn registered_defaults_are_explicitly_reparsable() {
    let registry = OptionRegistry::new();
    for definition in registry.all().values() {
        if !matches!(definition.default_value(), OptionValue::None) {
            let raw = definition.default_value().to_string();
            assert!(
                definition.parse_value(&raw).is_ok(),
                "default for '{}' is not accepted by its own definition",
                definition.name()
            );
        }
    }
}
