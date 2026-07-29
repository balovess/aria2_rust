//! Tests for the option module.

use std::collections::HashMap;

use serde_json::Value;

use super::types::{OptionCategory, OptionDef, OptionType, OptionValue};
use super::validator::{
    ChoiceValidator, DependencyChecker, OptionDefinition, OptionError, OptionValidator,
    RangeValidator, RegexValidator, UrlValidator,
};
use super::registry::OptionRegistry;

// ==================== Type Tests ====================

#[test]
fn test_option_type_display() {
    assert_eq!(OptionType::String.to_string(), "string");
    assert_eq!(OptionType::Boolean.to_string(), "boolean");
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
        default_value: OptionValue::Int(5),
        description: "Connections per download".into(),
        min: Some(1),
        max: Some(16),
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
fn test_option_def_parse_integer() {
    let def = OptionDef {
        name: "split".into(),
        opt_type: OptionType::Integer,
        min: Some(1),
        max: Some(16),
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
fn test_option_def_parse_boolean() {
    let def = OptionDef::new("verbose", OptionType::Boolean);
    assert!(def.parse_value("true").unwrap().as_bool().unwrap());
    assert!(def.parse_value("yes").unwrap().as_bool().unwrap());
    assert!(def.parse_value("1").unwrap().as_bool().unwrap());
    assert!(!def.parse_value("false").unwrap().as_bool().unwrap());
    assert!(!def.parse_value("no").unwrap().as_bool().unwrap());
    assert!(def.parse_value("invalid").is_err());
}

#[test]
fn test_option_def_parse_list() {
    let def = OptionDef::new("header", OptionType::List);
    let v = def.parse_value("X-Custom:foo,X-Bar:baz").unwrap();
    assert_eq!(v.as_list().unwrap().len(), 2);
}

#[test]
fn test_option_def_parse_empty_uses_default() {
    let def = OptionDef {
        name: "dir".into(),
        opt_type: OptionType::Path,
        default_value: OptionValue::Str("/tmp".into()),
        ..Default::default()
    };
    let v = def.parse_value("").unwrap();
    assert_eq!(v.as_str().unwrap(), "/tmp");
}

// ==================== Registry Tests ====================

#[test]
fn test_registry_creation() {
    let reg = OptionRegistry::new();
    assert!(reg.count() >= 60);
    assert!(reg.get("split").is_some());
    assert!(reg.get("nonexistent-option").is_none());
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
fn test_registry_defaults_are_valid() {
    let reg = OptionRegistry::new();
    for def in reg.all().values() {
        if !matches!(def.default_value(), OptionValue::None) {
            let parsed = def.parse_value(&def.default_value().to_string());
            assert!(
                parsed.is_ok(),
                "Default value for '{}' failed to re-parse: {:?}",
                def.name(),
                parsed.err()
            );
        }
    }
}

#[test]
fn test_default_registry() {
    let reg = OptionRegistry::default();
    assert!(reg.count() > 0);
}

// ==================== Validator Tests ====================

#[test]
fn test_range_validator_in_range() {
    let validator = RangeValidator::<i64>::new(1, 16);
    assert!(validator.validate("split", &Value::from(1)).is_ok());
    assert!(validator.validate("split", &Value::from(8)).is_ok());
    assert!(validator.validate("split", &Value::from(16)).is_ok());

    let float_validator = RangeValidator::<f64>::new(0.0, 1.0);
    assert!(float_validator.validate("ratio", &Value::from(0.5)).is_ok());

    let u64_validator = RangeValidator::<u64>::new(1024, 1024 * 1024);
    assert!(
        u64_validator
            .validate("size", &Value::from(4096u64))
            .is_ok()
    );
}

#[test]
fn test_range_validator_out_of_range() {
    let validator = RangeValidator::<i64>::new(1, 16);
    let result = validator.validate("split", &Value::from(0));
    assert!(result.is_err());
    match result.unwrap_err() {
        OptionError::OutOfRange { value, min, max } => {
            assert_eq!(value, "0");
            assert_eq!(min, "1");
            assert_eq!(max, "16");
        }
        other => panic!("Expected OutOfRange error, got {:?}", other),
    }
}

#[test]
fn test_choice_validator_enum() {
    let validator = ChoiceValidator::new(vec![
        "debug".to_string(),
        "info".to_string(),
        "warn".to_string(),
        "error".to_string(),
    ]);
    assert!(
        validator
            .validate("log-level", &Value::String("debug".into()))
            .is_ok()
    );
    assert!(
        validator
            .validate("log-level", &Value::String("verbose".into()))
            .is_err()
    );
}

#[test]
fn test_url_validator_malformed() {
    let validator = UrlValidator::new();
    assert!(
        validator
            .validate(
                "tracker",
                &Value::String("http://example.com:6969/announce".into())
            )
            .is_ok()
    );
    assert!(
        validator
            .validate("url", &Value::String("not-a-url".into()))
            .is_err()
    );
}

#[test]
fn test_regex_validator_pattern_match() {
    let validator = RegexValidator::new(r"^[a-zA-Z0-9.-]+:\d+$");
    assert!(
        validator
            .validate("proxy", &Value::String("proxy.example.com:8080".into()))
            .is_ok()
    );
    assert!(
        validator
            .validate("proxy", &Value::String("not-valid".into()))
            .is_err()
    );
}

#[test]
fn test_dependency_checker() {
    let mut checker = DependencyChecker::new();
    checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());
    checker.add_requirement("bt-enable-lpd".to_string(), "enable-dht".to_string());

    let mut opts = HashMap::new();
    opts.insert("ftp-pasv".to_string(), Value::Bool(true));
    opts.insert("ftp-port".to_string(), Value::from(8021));
    let errors = checker.check(&opts);
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_option_definition_validation() {
    let def = OptionDefinition {
        name: "max-connections",
        description: "Maximum connections per server",
        default_value: Value::from(16),
        validator: Some(Box::new(RangeValidator::<i64>::new(1, 32))),
    };
    assert!(def.validate(&Value::from(8)).is_ok());
    assert!(def.validate(&Value::from(0)).is_err());
}

#[test]
fn test_option_error_display() {
    let err = OptionError::TypeMismatch {
        expected: "integer".to_string(),
        got: "string".to_string(),
    };
    let msg = format!("{}", err);
    assert!(msg.contains("type mismatch"));
}
