use super::*;

// ==================== Type Tests ====================

#[test]
fn test_option_type_display() {
    assert_eq!(OptionType::String.to_string(), "string");
    assert_eq!(OptionType::Boolean.to_string(), "boolean");
    assert_eq!(OptionType::PiecePriority.to_string(), "piece-priority");
    assert_eq!(OptionType::Size.to_string(), "size");
}

#[test]
fn test_option_category_display() {
    assert_eq!(OptionCategory::General.to_string(), "general");
    assert_eq!(OptionCategory::BitTorrent.to_string(), "bittorrent");
}

#[test]
fn test_option_value_variants() {
    let s = OptionValue::Str("hello".into());
    assert_eq!(s.as_str().unwrap(), "hello");

    let n = OptionValue::Int(42);
    assert_eq!(n.as_i64().unwrap(), 42);

    let b = OptionValue::Bool(true);
    assert!(b.as_bool().unwrap());

    let l = OptionValue::List(vec!["a".into(), "b".into()]);
    assert_eq!(l.as_list().unwrap().len(), 2);

    let none = OptionValue::None;
    assert!(none.is_none());
}

#[test]
fn test_option_value_display() {
    assert_eq!(OptionValue::Str("test".into()).to_string(), "test");
    assert_eq!(OptionValue::Int(99).to_string(), "99");
    assert_eq!(OptionValue::Bool(true).to_string(), "true");
    assert_eq!(
        OptionValue::List(vec!["x".into(), "y".into()]).to_string(),
        "x,y"
    );
}

#[test]
fn test_option_value_to_json() {
    let v = OptionValue::Str("hello".into());
    let jv: serde_json::Value = (&v).into();
    assert_eq!(jv, "hello");

    let v2 = OptionValue::Int(123);
    let jv2: serde_json::Value = (&v2).into();
    assert_eq!(jv2, 123);

    let v3 = OptionValue::Bool(false);
    let jv3: serde_json::Value = (&v3).into();
    assert_eq!(jv3, false);

    let v4 = OptionValue::List(vec!["a".into()]);
    let jv4: serde_json::Value = (&v4).into();
    assert!(jv4.is_array());
}

#[test]
fn test_option_value_from_json() {
    let ov: OptionValue = serde_json::json!("test string").into();
    assert_eq!(ov.as_str().unwrap(), "test string");

    let ov2: OptionValue = serde_json::json!(42).into();
    assert_eq!(ov2.as_i64().unwrap(), 42);

    let ov3: OptionValue = serde_json::json!(true).into();
    assert!(ov3.as_bool().unwrap());

    let ov4: OptionValue = serde_json::json!(["a", "b"]).into();
    assert_eq!(ov4.as_list().unwrap().len(), 2);
}

#[test]
fn test_size_parsing() {
    assert_eq!(OptionValue::parse_size_str("100"), 100);
    assert_eq!(OptionValue::parse_size_str("1K"), 1024);
    assert_eq!(OptionValue::parse_size_str("2M"), 2 * 1024 * 1024);
    assert_eq!(OptionValue::parse_size_str("1G"), 1024u64 * 1024 * 1024);
    assert_eq!(OptionValue::parse_size_str("0"), 0);
}

#[test]
fn test_size_display() {
    assert!(OptionValue::to_size_string(500).contains("500"));
    assert!(OptionValue::to_size_string(2048).contains("K"));
    assert!(OptionValue::to_size_string(3 * 1024 * 1024).contains("M"));
}

#[test]
fn test_option_def_builder() {
    let def = OptionDef {
        name: "split".into(),
        opt_type: OptionType::Integer,
        short_name: Some('s'),
        default_value: OptionValue::Int(16),
        description: "Concurrent segment requests per download".into(),
        min: Some(1),
        max: Some(128),
        category: OptionCategory::HttpFtp,
        ..Default::default()
    };
    assert_eq!(def.name(), "split");
    assert_eq!(def.short_name(), Some('s'));
    assert_eq!(def.opt_type(), OptionType::Integer);
    assert!(!def.is_deprecated());
    assert!(!def.is_hidden());
}

#[test]
fn test_rpc_basic_auth_options_match_original_deprecation_metadata() {
    let registry = OptionRegistry::new();

    assert!(registry.get("rpc-user").unwrap().is_deprecated());
    assert!(registry.get("rpc-passwd").unwrap().is_deprecated());
    assert!(!registry.get("rpc-secret").unwrap().is_deprecated());
    assert!(
        !registry
            .get("rpc-secret")
            .unwrap()
            .is_exposed_in_aria2_rpc()
    );
}

#[test]
fn max_http_pipelining_is_explicitly_rejected_without_runtime_support() {
    let registry = OptionRegistry::new();

    let error = registry
        .parse_rpc_value("max-http-pipelining", &serde_json::json!(2))
        .expect_err("unsupported HTTP pipeline limits must not be silently stored");

    assert!(
        error.contains("max-http-pipelining"),
        "unexpected error: {error}"
    );
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_hidden_and_deprecated_option_sets_match_original_boundary() {
    use std::collections::BTreeSet;

    let registry = OptionRegistry::new();
    let hidden = registry
        .all()
        .values()
        .filter(|definition| definition.is_hidden())
        .map(|definition| definition.name().to_owned())
        .collect::<BTreeSet<_>>();
    let deprecated = registry
        .all()
        .values()
        .filter(|definition| definition.is_deprecated())
        .map(|definition| definition.name().to_owned())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        hidden,
        BTreeSet::from([
            "bt-keep-alive-interval".to_string(),
            "bt-request-timeout".to_string(),
            "bt-timeout".to_string(),
            "dht-listen-addr".to_string(),
            "dns-timeout".to_string(),
            "max-http-pipelining".to_string(),
            "optimize-concurrent-downloads-coeffA".to_string(),
            "optimize-concurrent-downloads-coeffB".to_string(),
            "peer-connection-timeout".to_string(),
            "select-least-used-host".to_string(),
            "startup-idle-time".to_string(),
        ])
    );
    assert_eq!(
        deprecated,
        BTreeSet::from([
            "enable-async-dns6".to_string(),
            "rpc-passwd".to_string(),
            "rpc-user".to_string(),
        ])
    );
}

#[test]
fn test_option_def_parse_integer() {
    let def = OptionDef {
        name: "split".into(),
        opt_type: OptionType::Integer,
        min: Some(1),
        max: Some(128),
        ..Default::default()
    };
    let v = def.parse_value("5").unwrap();
    assert_eq!(v.as_i64().unwrap(), 5);

    let err = def.parse_value("0");
    assert!(err.is_err());

    let err2 = def.parse_value("abc");
    assert!(err2.is_err());
}

#[test]
fn test_option_def_parse_integer_range_preserves_wire_value() {
    let def = OptionDef {
        name: "listen-port".into(),
        opt_type: OptionType::IntegerRange,
        min: Some(1024),
        max: Some(65535),
        ..Default::default()
    };

    assert_eq!(
        def.parse_value("6881-6999,7001").unwrap().as_str(),
        Some("6881-6999,7001")
    );
    assert!(def.parse_value("1023").is_err());
    assert!(def.parse_value("70000").is_err());
    assert!(def.parse_value("6881-").is_err());
    assert!(def.parse_value("6881-6999-7000").is_err());
}

#[test]
fn test_option_def_parse_index_out_is_cumulative_wire_text() {
    let def = OptionDef {
        name: "index-out".into(),
        opt_type: OptionType::IndexOut,
        cumulative_delimiter: Some("\n"),
        ..Default::default()
    };

    assert_eq!(
        def.parse_value("1=part.iso").unwrap().as_str(),
        Some("1=part.iso")
    );
    assert!(def.parse_value("part.iso").is_err());
    assert!(def.parse_value("1=").is_err());
}

#[test]
fn test_option_def_parse_piece_priority_matches_original_syntax() {
    let def = OptionDef {
        name: "bt-prioritize-piece".into(),
        opt_type: OptionType::PiecePriority,
        ..Default::default()
    };

    assert_eq!(
        def.parse_value("head=512K,tail").unwrap().as_str(),
        Some("head=512K,tail")
    );
    assert!(def.parse_value("rarest").is_err());
    assert!(def.parse_value("head=").is_err());
    assert!(def.parse_value("tail=1G").is_err());
}

#[test]
fn test_parse_piece_priority_uses_original_default_size() {
    assert_eq!(
        super::parse_piece_priority("head,tail=2M").unwrap(),
        vec![
            super::PiecePriorityRule::Head { size: 1024 * 1024 },
            super::PiecePriorityRule::Tail {
                size: 2 * 1024 * 1024
            },
        ]
    );
    assert!(super::parse_piece_priority("head,,tail").is_ok());
    assert!(super::parse_piece_priority("head=0").is_ok());
}

#[test]
fn test_parse_index_out_preserves_order_and_paths() {
    assert_eq!(
        super::parse_index_out("1=first.iso\r\n2=dir/second.iso").unwrap(),
        vec![
            (1, "first.iso".to_string()),
            (2, "dir/second.iso".to_string()),
        ]
    );
    assert!(super::parse_index_out("0=first.iso").is_ok());
    assert!(super::parse_index_out("part.iso").is_err());
    assert!(super::parse_index_out("1=").is_err());
}

#[test]
fn test_size_bounds_are_applied_by_the_definition() {
    let def = OptionDef {
        name: "piece-length".into(),
        opt_type: OptionType::Size,
        min: Some(1024 * 1024),
        max: Some(1024 * 1024 * 1024),
        ..Default::default()
    };

    assert!(def.parse_value("1M").is_ok());
    assert!(def.parse_value("512K").is_err());
    assert!(def.parse_value("2G").is_err());
}

#[test]
fn test_option_def_parse_boolean() {
    let def = OptionDef::new("verbose", OptionType::Boolean);
    assert!(def.parse_value("true").unwrap().as_bool().unwrap());
    assert!(!def.parse_value("false").unwrap().as_bool().unwrap());
    assert!(def.parse_value("").unwrap().as_bool().unwrap());
    assert!(def.parse_value("yes").is_err());
    assert!(def.parse_value("1").is_err());
    assert!(def.parse_value("no").is_err());
    assert!(def.parse_value("TRUE").is_err());
    assert!(def.parse_value("invalid").is_err());
}

#[test]
fn test_option_def_parse_list() {
    let def = OptionDef::new("header", OptionType::List);
    let v = def.parse_value("X-Custom:foo,X-Bar:baz").unwrap();
    assert_eq!(v.as_list().unwrap().len(), 2);
    assert_eq!(
        def.parse_value("").unwrap().as_list().unwrap(),
        &vec![String::new()]
    );
}

#[test]
fn test_empty_tracker_source_parses_as_empty_list() {
    let def = OptionDef::new("bt-tracker-source", OptionType::List);
    assert!(def.parse_value("").unwrap().as_list().unwrap().is_empty());
}

#[test]
fn test_option_def_parse_enum_rejects_unknown_choice() {
    let def = OptionDef {
        name: "uri-selector".into(),
        opt_type: OptionType::Enum,
        allowed_values: &["inorder", "feedback", "adaptive"],
        ..Default::default()
    };
    assert_eq!(
        def.parse_value("feedback").unwrap().as_str(),
        Some("feedback")
    );
    assert!(def.parse_value("unknown").is_err());
}

#[test]
fn test_option_def_parse_explicit_empty_is_not_a_default_request() {
    let def = OptionDef {
        name: "dir".into(),
        opt_type: OptionType::Path,
        default_value: OptionValue::Str("/tmp".into()),
        ..Default::default()
    };
    let v = def.parse_value("").unwrap();
    assert_eq!(v.as_str().unwrap(), "");
    assert_eq!(def.parse_default_value().unwrap().as_str(), Some("/tmp"));
}

#[test]
fn test_rpc_empty_values_use_explicit_option_semantics() {
    let registry = OptionRegistry::new();

    assert_eq!(
        registry
            .parse_rpc_value("dir", &serde_json::json!(""))
            .unwrap()
            .as_str(),
        Some("")
    );
    assert!(
        registry
            .parse_rpc_value("split", &serde_json::json!(""))
            .is_err()
    );
    assert!(
        registry
            .parse_rpc_value("rpc-secret", &serde_json::json!(""))
            .is_err()
    );
    assert!(!registry.get("rpc-secret").unwrap().allow_empty);
}

#[test]
fn test_option_value_parse_size_rejects_invalid_input() {
    assert_eq!(
        OptionValue::parse_size_str_checked("1.5M").unwrap(),
        1_572_864
    );
    assert!(OptionValue::parse_size_str_checked("badvalue").is_err());
    assert!(OptionValue::parse_size_str_checked("-1K").is_err());
}
