//! Tests for OptionHandler.

#[cfg(test)]
mod tests {
    use super::super::apply::OptionHandlerApply;
    use super::super::{OptionHandler, built_in_defaults, detect_value_type};
    use crate::config::option::OptionValue;

    #[test]
    fn test_defaults_populated() {
        // All defaults should be present after new()
        let handler = OptionHandler::new();
        let expected_count = built_in_defaults().len();
        assert_eq!(handler.default_count(), expected_count);
        assert!(handler.default_count() > 0);

        // Verify specific known defaults
        assert_eq!(handler.get("dir").as_str().unwrap_or(""), ".");
        assert_eq!(handler.get("split").as_usize(), 5);
        assert_eq!(handler.get("max-concurrent-downloads").as_usize(), 5);
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 1);
        assert_eq!(handler.get("min-split-size").as_usize(), 1_048_576);
        assert!(handler.get("continue").as_bool().unwrap_or(false));
        assert!(!handler.get("quiet").as_bool().unwrap_or(false));
        assert_eq!(handler.get("seed-ratio").as_f64().unwrap_or(0.0), 0.0);
        assert_eq!(handler.get("rpc-listen-port").as_usize(), 6800);
        assert_eq!(
            handler.get("console-log-level").as_str().unwrap_or(""),
            "info"
        );
    }

    #[test]
    fn test_set_get_roundtrip() {
        let mut handler = OptionHandler::new();

        // Set and retrieve various types
        handler.set("dir", OptionValue::Str("/tmp/downloads".into()));
        assert_eq!(handler.get("dir").as_str().unwrap_or(""), "/tmp/downloads");

        handler.set("split", OptionValue::Usize(16));
        assert_eq!(handler.get("split").as_usize(), 16);

        handler.set("seed-ratio", OptionValue::Float(2.5));
        assert!((handler.get("seed-ratio").as_f64().unwrap_or(0.0) - 2.5).abs() < f64::EPSILON);

        handler.set("quiet", OptionValue::Bool(true));
        assert!(handler.get("quiet").as_bool().unwrap_or(false));

        handler.set(
            "header",
            OptionValue::List(vec!["X-Custom: foo".into(), "X-Bar: baz".into()]),
        );
        assert_eq!(handler.get("header").as_str_vec().len(), 2);

        // Overwrite: second set wins
        handler.set("split", OptionValue::Usize(32));
        assert_eq!(handler.get("split").as_usize(), 32);

        // Unknown key returns None variant
        assert!(handler.get("nonexistent-key").is_none());
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_load_config_file() {
        let mut handler = OptionHandler::new();

        // Build sample .aria2rc content
        let config_content = r#"
# This is a comment
dir="/home/user/downloads"
split=16
max-connection-per-server=8
quiet=true
seed-ratio=1.5
custom-list=['header1', 'header2', 'header3']
bool-flag=yes
number-key=42
float-key=3.14

# Another comment
allow-overwrite=false
"#;

        // Write to temp file
        let tmp_dir = std::env::temp_dir();
        let config_path = tmp_dir.join(format!("aria2_test_config_{}.aria2rc", std::process::id()));
        std::fs::write(&config_path, config_content).expect("Failed to write temp config");

        // Load config file
        let result = handler.load_config_file(&config_path);
        assert!(
            result.is_ok(),
            "load_config_file should succeed: {:?}",
            result.err()
        );

        // Verify loaded values override defaults
        assert_eq!(
            handler.get("dir").as_str().unwrap_or(""),
            "/home/user/downloads"
        );
        assert_eq!(handler.get("split").as_usize(), 16);
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 8);
        assert!(handler.get("quiet").as_bool().unwrap_or(false));
        assert!((handler.get("seed-ratio").as_f64().unwrap_or(0.0) - 1.5).abs() < f64::EPSILON);
        assert!(!handler.get("allow-overwrite").as_bool().unwrap_or(true));

        // Verify list parsing
        let list_val = handler.get("custom-list");
        assert_eq!(list_val.as_str_vec().len(), 3);
        assert_eq!(list_val.as_str_vec()[0], "header1");

        // Verify auto-detected types
        assert!(handler.get("bool-flag").as_bool().unwrap_or(false)); // yes -> true
        assert_eq!(handler.get("number-key").as_usize(), 42);
        let float_val = handler.get("float-key").as_f64().unwrap_or(0.0);
        assert!((float_val - 3.14).abs() < f64::EPSILON);

        // Defaults should still be intact for unmentioned keys
        assert_eq!(handler.get("rpc-listen-port").as_usize(), 6800);

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[test]
    fn test_apply_args_overrides_config() {
        let mut handler = OptionHandler::new();

        // First load config file with some values
        let config_content = r#"
dir=/config/dir
split=4
quiet=false
"#;
        let tmp_dir = std::env::temp_dir();
        let config_path = tmp_dir.join(format!(
            "aria2_test_override_{}.aria2rc",
            std::process::id()
        ));
        std::fs::write(&config_path, config_content).expect("Failed to write config");
        handler
            .load_config_file(&config_path)
            .expect("Should load config");

        // Verify config values loaded
        assert_eq!(handler.get("dir").as_str().unwrap_or(""), "/config/dir");
        assert_eq!(handler.get("split").as_usize(), 4);
        assert!(!handler.get("quiet").as_bool().unwrap_or(false));

        // Now apply CLI args (should override config)
        let cli_args: Vec<String> = vec![
            "--dir=/cli/dir".to_string(),
            "--split=12".to_string(),
            "--quiet".to_string(), // flag without value -> bool true
            "--max-connection-per-server=8".to_string(),
            "--seed-ratio=2.0".to_string(),
            "--no-continue".to_string(), // --no-key pattern -> bool false
        ];
        handler.apply_args(&cli_args);

        // CLI args should win over config
        assert_eq!(handler.get("dir").as_str().unwrap_or(""), "/cli/dir");
        assert_eq!(handler.get("split").as_usize(), 12);
        assert!(handler.get("quiet").as_bool().unwrap_or(false)); // CLI flag overrides config
        assert_eq!(handler.get("max-connection-per-server").as_usize(), 8);
        assert!((handler.get("seed-ratio").as_f64().unwrap_or(0.0) - 2.0).abs() < f64::EPSILON);
        assert!(!handler.get("continue").as_bool().unwrap_or(true)); // --no-continue

        // Cleanup
        let _ = std::fs::remove_file(&config_path);
    }

    #[test]
    fn test_to_download_options() {
        let mut handler = OptionHandler::new();

        // Set values that map to DownloadOptions fields
        handler.set("split", OptionValue::Usize(8));
        handler.set("max-connection-per-server", OptionValue::Usize(4));
        handler.set("max-download-limit", OptionValue::Usize(102400));
        handler.set("max-upload-limit", OptionValue::Usize(51200));
        handler.set("dir", OptionValue::Str("/data".to_string()));
        handler.set("out", OptionValue::Str("output.bin".to_string()));
        handler.set("seed-time", OptionValue::Usize(300));
        handler.set("seed-ratio", OptionValue::Float(2.0));

        let opts = handler.to_download_options();

        // Verify conversion produced correct struct
        assert_eq!(opts.split, Some(8));
        assert_eq!(opts.max_connection_per_server, Some(4));
        assert_eq!(opts.max_download_limit, Some(102400));
        assert_eq!(opts.max_upload_limit, Some(51200));
        assert_eq!(opts.dir, Some("/data".to_string()));
        assert_eq!(opts.out, Some("output.bin".to_string()));
        assert_eq!(opts.seed_time, Some(300.0));
        assert_eq!(opts.seed_ratio, Some(2.0));

        // Default values (non-zero) should be preserved in DownloadOptions
        let handler2 = OptionHandler::new();
        let opts2 = handler2.to_download_options();
        assert_eq!(opts2.split, Some(5)); // default split=5 which is > 0
        assert_eq!(opts2.max_connection_per_server, Some(1)); // default is 1 (aria2-next)
        assert_eq!(opts2.dir, Some(".".to_string())); // default dir is "."
        assert_eq!(opts2.out, None); // "out" has no default -> None

        // Verify reset_to_default works
        handler.reset_to_default("split");
        assert_eq!(handler.get("split").as_usize(), 5); // back to default
        assert!(!handler.is_explicitly_set("split"));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_detect_value_type_edge_cases() {
        // Test various auto-detection scenarios
        assert_eq!(detect_value_type("true"), Some(OptionValue::Bool(true)));
        assert_eq!(detect_value_type("false"), Some(OptionValue::Bool(false)));
        assert_eq!(detect_value_type("yes"), Some(OptionValue::Bool(true)));
        assert_eq!(detect_value_type("no"), Some(OptionValue::Bool(false)));
        assert_eq!(detect_value_type("42"), Some(OptionValue::Usize(42)));
        assert_eq!(detect_value_type("-10"), Some(OptionValue::Int(-10)));
        let detected = detect_value_type("3.14159")
            .unwrap()
            .as_f64()
            .unwrap_or(0.0);
        assert!((detected - 3.14159).abs() < 0.001); // use full precision to avoid lint
        assert_eq!(
            detect_value_type("\"quoted string\""),
            Some(OptionValue::Str("quoted string".into()))
        );
        assert_eq!(
            detect_value_type("['a','b','c']"),
            Some(OptionValue::List(vec!["a".into(), "b".into(), "c".into()]))
        );
        assert_eq!(detect_value_type(""), Some(OptionValue::None));
        assert_eq!(
            detect_value_type("plain_text"),
            Some(OptionValue::Str("plain_text".into()))
        );
    }

    #[test]
    fn test_option_value_display() {
        assert_eq!(OptionValue::Bool(true).to_string(), "true");
        assert_eq!(OptionValue::Usize(42).to_string(), "42");
        assert_eq!(OptionValue::Int(-10).to_string(), "-10");
        assert_eq!(
            format!("{:.2}", {
                #[allow(clippy::approx_constant)]
                OptionValue::Float(3.14).to_string().parse::<f64>().unwrap()
            }),
            "3.14"
        ); // approximate
        assert_eq!(OptionValue::Str("hello".to_string()).to_string(), "hello");
        assert_eq!(
            OptionValue::List(vec!["a".into(), "b".into()]).to_string(),
            "a,b"
        );
        assert_eq!(OptionValue::None.to_string(), "");
    }

    #[test]
    fn test_to_map_includes_all() {
        let mut handler = OptionHandler::new();
        handler.set("custom-key", OptionValue::Str("custom-value".into()));

        let map = handler.to_map();
        // Should include all defaults plus custom key
        assert!(map.contains_key("dir"));
        assert!(map.contains_key("split"));
        assert!(map.contains_key("custom-key"));
        assert_eq!(
            map.get("custom-key").unwrap().as_str().unwrap_or(""),
            "custom-value"
        );
        // Map size >= defaults count
        assert!(map.len() >= built_in_defaults().len());
    }
}
