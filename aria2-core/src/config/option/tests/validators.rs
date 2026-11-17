use super::*;

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
