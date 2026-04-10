/// Standalone integration tests for the option_validator module.
///
/// This file is separate from the main module to avoid compilation issues
/// with other test files that have pre-existing errors in the codebase.
#[cfg(test)]
mod standalone_option_validator_tests {
    use aria2_core::config::option_validator::*;
    use serde_json::{json, Value};
    use std::collections::HashMap;

    #[test]
    fn test_range_validator_in_range() {
        let validator = RangeValidator::<i64>::new(1, 16);

        // Values within range should pass
        assert!(validator.validate("split", &Value::from(1)).is_ok());
        assert!(validator.validate("split", &Value::from(8)).is_ok());
        assert!(validator.validate("split", &Value::from(16)).is_ok());

        // Float range validator
        let float_validator = RangeValidator::<f64>::new(0.0, 1.0);
        assert!(float_validator.validate("ratio", &Value::from(0.5)).is_ok());
        assert!(float_validator.validate("ratio", &Value::from(0.0)).is_ok());
        assert!(float_validator.validate("ratio", &Value::from(1.0)).is_ok());
    }

    #[test]
    fn test_range_validator_out_of_range() {
        let validator = RangeValidator::<i64>::new(1, 16);

        // Below minimum
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

        // Above maximum
        let result = validator.validate("split", &Value::from(20));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::OutOfRange { value, .. } => {
                assert_eq!(value, "20");
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

        // Valid choices
        assert!(validator.validate("log-level", &json!("info")).is_ok());
        assert!(validator.validate("log-level", &json!("debug")).is_ok());

        // Invalid choice
        let result = validator.validate("log-level", &json!("verbose"));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidChoice { value, allowed } => {
                assert_eq!(value, "verbose");
                assert_eq!(allowed.len(), 4);
            }
            other => panic!("Expected InvalidChoice error, got {:?}", other),
        }
    }

    #[test]
    fn test_url_validator_malformed() {
        let validator = UrlValidator::new();

        // Valid URLs
        assert!(validator
            .validate("tracker", &json!("http://example.com:6969/announce"))
            .is_ok());
        assert!(validator
            .validate("tracker", &json!("https://tracker.example.com/announce"))
            .is_ok());

        // Malformed URLs
        let result = validator.validate("url", &json!("not-a-url"));
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidUrl { url, reason } => {
                assert_eq!(url, "not-a-url");
                assert!(reason.contains("missing scheme"));
            }
            other => panic!("Expected InvalidUrl error, got {:?}", other),
        }

        // Empty URL
        assert!(validator.validate("url", &json!("")).is_err());
    }

    #[test]
    fn test_mutual_exclusion_detection() {
        let mut checker = DependencyChecker::new();

        // Add mutual exclusion: ftp-pasv and ftp-port cannot coexist
        checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());

        // Case 1: Only one set - should pass
        let mut opts1 = HashMap::new();
        opts1.insert("ftp-pasv".to_string(), json!(true));
        let errors1 = checker.check(&opts1);
        assert!(errors1.is_empty());

        // Case 2: Both set - should detect conflict
        let mut opts2 = HashMap::new();
        opts2.insert("ftp-pasv".to_string(), json!(true));
        opts2.insert("ftp-port".to_string(), json!(8021));
        let errors2 = checker.check(&opts2);
        assert_eq!(errors2.len(), 1);
        match &errors2[0] {
            OptionError::DependencyConflict {
                option,
                conflicts_with,
            } => {
                assert_eq!(option, "ftp-pasv");
                assert_eq!(conflicts_with, "ftp-port");
            }
            other => panic!("Expected DependencyConflict, got {:?}", other),
        }
    }

    #[test]
    fn test_dependency_satisfaction() {
        let mut checker = DependencyChecker::new();

        // bt-enable-lpd requires enable-dht
        checker.add_requirement(
            "bt-enable-lpd".to_string(),
            "enable-dht".to_string(),
        );

        // Case 1: Both set - satisfied
        let mut opts1 = HashMap::new();
        opts1.insert("bt-enable-lpd".to_string(), json!(true));
        opts1.insert("enable-dht".to_string(), json!(true));
        let errors1 = checker.check(&opts1);
        assert!(errors1.is_empty());

        // Case 2: Dependent set but required missing - violation
        let mut opts2 = HashMap::new();
        opts2.insert("bt-enable-lpd".to_string(), json!(true));
        let errors2 = checker.check(&opts2);
        assert_eq!(errors2.len(), 1);
        match &errors2[0] {
            OptionError::MissingDependency {
                option,
                requires,
            } => {
                assert_eq!(option, "bt-enable-lpd");
                assert_eq!(requires, "enable-dht");
            }
            other => panic!("Expected MissingDependency, got {:?}", other),
        }
    }

    #[test]
    fn test_default_value_fallback() {
        // Test with a null default value
        let def_with_null = OptionDefinition {
            name: "optional-param",
            description: "An optional parameter",
            default_value: Value::Null,
            validator: None,
        };

        let fallback = json!("fallback-value");
        // Should return the fallback when default is Null
        assert_eq!(
            def_with_null.get_default_or_fallback(&fallback),
            &json!("fallback-value")
        );

        // Test with a non-null default value
        let def_with_value = OptionDefinition {
            name: "required-param",
            description: "A required parameter",
            default_value: json!("default-value"),
            validator: None,
        };

        let fallback = json!("should-not-use-this");
        // Should return the actual default (not the fallback)
        assert_eq!(
            def_with_value.get_default_or_fallback(&fallback),
            &json!("default-value")
        );
    }

    #[test]
    fn test_option_error_display() {
        // TypeMismatch
        let err = OptionError::TypeMismatch {
            expected: "integer".to_string(),
            got: "string".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("type mismatch"));

        // OutOfRange
        let err = OptionError::OutOfRange {
            value: "0".to_string(),
            min: "1".to_string(),
            max: "16".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("out of range"));

        // InvalidChoice
        let err = OptionError::InvalidChoice {
            value: "verbose".to_string(),
            allowed: vec!["debug".to_string(), "info".to_string()],
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid choice"));

        // InvalidUrl
        let err = OptionError::InvalidUrl {
            url: "not-a-url".to_string(),
            reason: "missing scheme separator".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid URL"));

        // InvalidPath
        let err = OptionError::InvalidPath {
            path: "/nonexistent".to_string(),
            reason: "path does not exist".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("invalid path"));

        // PatternMismatch
        let err = OptionError::PatternMismatch {
            value: "abc".to_string(),
            pattern: r"^\d+$".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("does not match pattern"));

        // DependencyConflict
        let err = OptionError::DependencyConflict {
            option: "opt-a".to_string(),
            conflicts_with: "opt-b".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("conflicts"));

        // MissingDependency
        let err = OptionError::MissingDependency {
            option: "child-opt".to_string(),
            requires: "parent-opt".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("requires"));
    }

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        // Verify that validators can be shared across threads
        let validator: Arc<dyn OptionValidator> = Arc::new(RangeValidator::<i64>::new(1, 16));
        let mut handles = Vec::new();

        for i in 0..4 {
            let v = Arc::clone(&validator);
            handles.push(thread::spawn(move || {
                let val = if i % 2 == 0 { 8 } else { 20 };
                v.validate("test", &Value::from(val))
            }));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.join().unwrap();
            if i % 2 == 0 {
                assert!(result.is_ok(), "Thread {} should pass", i);
            } else {
                assert!(result.is_err(), "Thread {} should fail", i);
            }
        }
    }

    #[test]
    fn test_regex_validator_pattern_match() {
        // Host:port pattern
        let validator = RegexValidator::new(r"^[a-zA-Z0-9.-]+:\d+$");

        assert!(validator
            .validate("proxy", &json!("proxy.example.com:8080"))
            .is_ok());
        assert!(validator.validate("proxy", &json!("localhost:3128")).is_ok());

        // Should fail for invalid input
        assert!(validator.validate("proxy", &json!("not-valid")).is_err());

        // Wrong type should fail
        assert!(validator.validate("proxy", &json!(42)).is_err());
    }

    #[test]
    fn test_path_validator_nonexistent_path() {
        let validator = PathValidator::new(true, false);

        // This path definitely shouldn't exist
        let result = validator.validate(
            "dir",
            &json!("/nonexistent/path/that/does/not/exist/xyz123"),
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            OptionError::InvalidPath { path, reason } => {
                assert!(path.contains("nonexistent"));
                assert!(reason.contains("does not exist"));
            }
            other => panic!("Expected InvalidPath error, got {:?}", other),
        }
    }

    #[test]
    fn test_option_definition_validation() {
        // Option with validator
        let def = OptionDefinition {
            name: "max-connections",
            description: "Maximum connections per server",
            default_value: json!(16),
            validator: Some(Box::new(RangeValidator::<i64>::new(1, 32))),
        };

        // Valid value should pass
        assert!(def.validate(&json!(8)).is_ok());
        assert!(def.validate(&json!(1)).is_ok());
        assert!(def.validate(&json!(32)).is_ok());

        // Invalid value should fail
        assert!(def.validate(&json!(0)).is_err());
        assert!(def.validate(&json!(100)).is_err());

        // Option without validator (backward compatible)
        let def_no_validator = OptionDefinition {
            name: "some-option",
            description: "An option without validation",
            default_value: json!("default"),
            validator: None,
        };

        // Should always pass validation (backward compatible)
        assert!(def_no_validator.validate(&json!(42)).is_ok());
        assert!(def_no_validator.validate(&json!("anything")).is_ok());
    }

    #[test]
    fn test_dependency_checker_multiple_rules() {
        let mut checker = DependencyChecker::new();

        // Add multiple mutual exclusions
        checker.add_mutual_exclusion("ftp-pasv".to_string(), "ftp-port".to_string());
        checker.add_mutual_exclusion("option-a".to_string(), "option-b".to_string());

        // Add multiple requirements
        checker.add_requirement("feature-x".to_string(), "base-feature".to_string());
        checker.add_requirement("feature-y".to_string(), "base-feature".to_string());

        assert_eq!(checker.mutual_exclusion_count(), 2);
        assert_eq!(checker.requirement_count(), 2);

        // Create options that violate all rules
        let mut opts = HashMap::new();
        opts.insert("ftp-pasv".to_string(), json!(true));
        opts.insert("ftp-port".to_string(), json!(8021));
        opts.insert("option-a".to_string(), json!(true));
        opts.insert("option-b".to_string(), json!(true));
        opts.insert("feature-x".to_string(), json!(true));
        opts.insert("feature-y".to_string(), json!(true));

        let errors = checker.check(&opts);
        // Should detect: 2 mutual exclusion conflicts + 2 missing dependencies = 4 errors
        assert_eq!(errors.len(), 4);
    }
}
