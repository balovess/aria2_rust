use super::*;

// ==================== Registry Tests ====================

#[test]
fn test_registry_creation() {
    let reg = OptionRegistry::new();
    assert!(reg.count() >= 60);
    assert!(reg.get("split").is_some());
    assert!(reg.get("nonexistent-option").is_none());
}

#[test]
fn sftp_options_remain_registered_with_protocol_defaults() {
    let reg = OptionRegistry::new();

    for name in [
        "ssh-host-key-md",
        "sftp-connect-timeout",
        "sftp-read-timeout",
        "sftp-compression",
        "sftp-keepalive-interval",
        "sftp-preferred-ciphers",
    ] {
        assert_eq!(
            reg.get(name)
                .expect("SFTP option must be registered")
                .is_supported(),
            cfg!(feature = "sftp"),
            "SFTP option support must match the enabled build feature: {name}"
        );
    }

    assert_eq!(
        reg.get("sftp-connect-timeout").unwrap().default_value,
        OptionValue::Int(aria2_protocol::DEFAULT_SFTP_CONNECT_TIMEOUT_SECS as i64)
    );
    assert_eq!(
        reg.get("sftp-read-timeout").unwrap().default_value,
        OptionValue::Int(aria2_protocol::DEFAULT_SFTP_READ_TIMEOUT_SECS as i64)
    );
    assert_eq!(
        reg.get("sftp-keepalive-interval").unwrap().default_value,
        OptionValue::Int(aria2_protocol::DEFAULT_SFTP_KEEPALIVE_INTERVAL_SECS as i64)
    );
}

#[test]
#[should_panic(expected = "duplicate configuration option 'duplicate'")]
fn test_registry_rejects_duplicate_definitions() {
    let mut reg = OptionRegistry::new();
    reg.register(OptionDef {
        owner: OptionOwner::Application,
        ..OptionDef::new("duplicate", OptionType::String)
    });
    reg.register(OptionDef {
        owner: OptionOwner::Application,
        ..OptionDef::new("duplicate", OptionType::Boolean)
    });
}

#[test]
#[should_panic(expected = "duplicate short option '-s'")]
fn test_registry_rejects_duplicate_short_options() {
    let mut reg = OptionRegistry::new();
    reg.register(OptionDef {
        name: "first-short-option".into(),
        short_name: Some('s'),
        owner: OptionOwner::Application,
        ..OptionDef::new("first-short-option", OptionType::String)
    });
    reg.register(OptionDef {
        name: "second-short-option".into(),
        short_name: Some('s'),
        owner: OptionOwner::Application,
        ..OptionDef::new("second-short-option", OptionType::String)
    });
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_bt_tracker_definition_matches_original_overwrite_semantics() {
    let reg = OptionRegistry::new();
    let def = reg
        .get("bt-tracker")
        .expect("bt-tracker must be registered");

    assert_eq!(def.opt_type(), OptionType::List);
    assert_eq!(def.cumulative_delimiter, None);
}

#[test]
fn test_registry_by_category() {
    let reg = OptionRegistry::new();
    let general = reg.by_category(OptionCategory::General);
    let rpc = reg.by_category(OptionCategory::Rpc);
    assert!(!general.is_empty());
    #[cfg(feature = "bittorrent")]
    {
        let bt = reg.by_category(OptionCategory::BitTorrent);
        assert!(!bt.is_empty());
    }
    assert!(!rpc.is_empty());
}

#[test]
fn test_rpc_cors_domain_is_unset_by_default() {
    let reg = OptionRegistry::new();
    let def = reg
        .get("rpc-cors-domain")
        .expect("rpc-cors-domain must be registered");

    assert!(matches!(def.default_value(), OptionValue::None));
}

#[test]
fn test_registry_defaults_are_valid() {
    let reg = OptionRegistry::new();
    for def in reg.all().values() {
        if !matches!(def.default_value(), OptionValue::None) {
            if def.is_supported() {
                let parsed = def.parse_value(&def.default_value().to_string());
                assert!(
                    parsed.is_ok(),
                    "Default value for '{}' failed to re-parse: {:?}",
                    def.name(),
                    parsed.err()
                );
            } else {
                assert!(
                    def.parse_default_value().is_none(),
                    "unsupported option '{}' must not expose a runtime default",
                    def.name()
                );
            }
        }
    }
}

#[test]
fn test_registry_identity_defaults_match_protocol_library() {
    let registry = OptionRegistry::new();
    assert_eq!(
        registry
            .get("user-agent")
            .unwrap()
            .default_value()
            .to_string(),
        aria2_protocol::identity::DEFAULT_USER_AGENT
    );
    #[cfg(feature = "bittorrent")]
    {
        assert_eq!(
            registry
                .get("peer-agent")
                .unwrap()
                .default_value()
                .to_string(),
            aria2_protocol::identity::DEFAULT_PEER_AGENT
        );
        assert_eq!(
            registry
                .get("peer-id-prefix")
                .unwrap()
                .default_value()
                .to_string(),
            aria2_protocol::identity::DEFAULT_PEER_ID_PREFIX
        );
    }
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_piece_priority_has_no_synthetic_default() {
    let registry = OptionRegistry::new();

    assert!(matches!(
        registry
            .get("bt-prioritize-piece")
            .expect("piece-priority option must be registered")
            .default_value(),
        OptionValue::None
    ));
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_seed_time_has_no_default_but_preserves_explicit_zero() {
    let registry = OptionRegistry::new();
    let definition = registry
        .get("seed-time")
        .expect("seed-time option must be registered");

    assert!(matches!(definition.default_value(), OptionValue::None));
    assert_eq!(definition.parse_value("0").unwrap().as_f64(), Some(0.0));
}

#[test]
fn unsupported_concurrency_optimization_options_are_explicitly_rejected() {
    let registry = OptionRegistry::new();
    for name in [
        "optimize-concurrent-downloads",
        "optimize-concurrent-downloads-coeffA",
        "optimize-concurrent-downloads-coeffB",
    ] {
        let definition = registry.get(name).expect("option must remain discoverable");
        assert!(!definition.is_supported());
        assert!(definition.parse_default_value().is_none());
        assert!(definition.parse_value("1").is_err());
    }
}

#[test]
fn multiple_interface_is_a_supported_process_network_option() {
    let registry = OptionRegistry::new();
    let definition = registry
        .get("multiple-interface")
        .expect("multiple-interface must remain discoverable");
    assert!(definition.is_supported());
    assert_eq!(
        definition
            .parse_value("127.0.0.1,127.0.0.2")
            .unwrap()
            .as_str(),
        Some("127.0.0.1,127.0.0.2")
    );
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_registry_parses_rpc_wire_values_through_one_typed_seam() {
    let reg = OptionRegistry::new();

    assert_eq!(
        reg.parse_rpc_value("split", &serde_json::json!(4))
            .unwrap()
            .as_i64(),
        Some(4)
    );
    assert_eq!(
        reg.parse_rpc_value("max-retries", &serde_json::json!(7))
            .unwrap()
            .as_i64(),
        Some(7)
    );
    assert_eq!(
        reg.parse_rpc_value("allow-overwrite", &serde_json::json!(true))
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        reg.parse_rpc_value("uri-selector", &serde_json::json!("adaptive"))
            .unwrap()
            .as_str(),
        Some("adaptive")
    );
    assert!(
        reg.parse_rpc_value("uri-selector", &serde_json::json!("unsupported"))
            .is_err()
    );
    assert!(
        reg.parse_rpc_value("split", &serde_json::json!({"value": 4}))
            .is_err()
    );
    assert!(
        reg.parse_rpc_value("split", &serde_json::json!([4]))
            .is_err()
    );
    assert!(
        reg.parse_rpc_value("header", &serde_json::json!(["X-Test: 1"]))
            .is_ok()
    );
    assert_eq!(
        reg.parse_rpc_value("listen-port", &serde_json::json!("6881-6999"))
            .unwrap()
            .as_str(),
        Some("6881-6999")
    );
    assert_eq!(
        reg.parse_rpc_value("bt-prioritize-piece", &serde_json::json!("head=2M,tail"))
            .unwrap()
            .as_str(),
        Some("head=2M,tail")
    );
    assert!(
        reg.parse_rpc_value("bt-prioritize-piece", &serde_json::json!("rarest"))
            .is_err()
    );
    assert!(
        reg.parse_rpc_value("select-file", &serde_json::json!("0"))
            .is_err()
    );
    assert!(
        reg.parse_rpc_value("index-out", &serde_json::json!("1=file.iso"))
            .is_ok()
    );
}

#[test]
fn test_default_registry() {
    let reg = OptionRegistry::default();
    assert!(reg.count() > 0);
}

#[test]
fn max_concurrent_zero_is_valid_unlimited_value() {
    let registry = OptionRegistry::new();
    let definition = registry
        .get("max-concurrent-downloads")
        .expect("max-concurrent-downloads must be registered");

    assert_eq!(definition.min, Some(0));
    assert_eq!(definition.max, Some(u32::MAX as u64));
    assert_eq!(definition.parse_value("0").unwrap().as_i64(), Some(0));
    assert!(
        definition
            .parse_value(&(u64::from(u32::MAX) + 1).to_string())
            .is_err()
    );
    assert_eq!(
        registry
            .parse_rpc_value("max-concurrent-downloads", &serde_json::json!(0))
            .unwrap()
            .as_i64(),
        Some(0)
    );

    let mut parser = crate::config::ConfigParser::new();
    parser.parse_cli_args(&["--max-concurrent-downloads=0"]);
    assert!(!parser.has_errors(), "CLI errors: {:?}", parser.errors());
    assert_eq!(parser.get_i64("max-concurrent-downloads"), Some(0));
}
