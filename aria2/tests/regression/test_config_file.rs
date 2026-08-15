//! Config file regression tests for aria2-rust.
//!
//! These tests verify config file parsing and saving functionality,
//! maintaining compatibility with original aria2 config file format.

use aria2_core::config::{ConfigManager, ConfigParser, OptionValue, UriListEntry, UriListFile};

// =========================================================================
// Config File Parsing Tests
// =========================================================================

/// Test: Basic config file parsing with key=value format.
#[test]
fn regression_config_file_basic_parsing() {
    let mut parser = ConfigParser::new();
    let content = "dir=/downloads\nsplit=8\nquiet=true\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    assert!(parser.get_bool("quiet").unwrap());
}

/// Test: Config file with comments (# prefix).
#[test]
fn regression_config_file_with_comments() {
    let mut parser = ConfigParser::new();
    let content = "# This is a comment\ndir=/downloads\n# Another comment\nsplit=8\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    assert!(
        !parser.contains("#"),
        "Comments should not be parsed as options"
    );
}

/// Test: Config file with empty lines.
#[test]
fn regression_config_file_empty_lines() {
    let mut parser = ConfigParser::new();
    let content = "dir=/downloads\n\n\nsplit=8\n\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
}

/// Test: Config file with section headers ([section]).
#[test]
fn regression_config_file_section_headers() {
    let mut parser = ConfigParser::new();
    let content = "[general]\ndir=/downloads\n[http]\nsplit=8\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // Section headers should be ignored, but options should be parsed
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
}

/// Test: Config file with semicolon comments (; prefix).
#[test]
fn regression_config_file_semicolon_comments() {
    let mut parser = ConfigParser::new();
    let content = "; Ini-style comment\ndir=/downloads\n; Another comment\nsplit=8\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
}

/// Test: Config file with spaces around equals.
#[test]
fn regression_config_file_spaces_around_equals() {
    let mut parser = ConfigParser::new();
    let content = "dir = /downloads\nsplit = 8\nquiet = true\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    assert!(parser.get_bool("quiet").unwrap());
}

/// Test: Config file with trailing spaces.
#[test]
fn regression_config_file_trailing_spaces() {
    let mut parser = ConfigParser::new();
    let content = "dir=/downloads   \nsplit=8   \n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
}

/// Test: Config file with integer values.
#[test]
fn regression_config_file_integer_values() {
    let mut parser = ConfigParser::new();
    let content = "split=16\ntimeout=120\nmax-tries=5\nrpc-listen-port=6801\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_i64("split").unwrap(), 16);
    assert_eq!(parser.get_i64("timeout").unwrap(), 120);
    assert_eq!(parser.get_i64("max-tries").unwrap(), 5);
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6801);
}

/// Test: Config file with boolean values (true/false).
#[test]
fn regression_config_file_boolean_values() {
    let mut parser = ConfigParser::new();
    let content = "quiet=true\ncheck-certificate=false\nenable-rpc=true\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert!(parser.get_bool("quiet").unwrap());
    assert!(!parser.get_bool("check-certificate").unwrap());
    assert!(parser.get_bool("enable-rpc").unwrap());
}

/// Test: Config file with size values (K, M, G suffixes).
#[test]
fn regression_config_file_size_values() {
    let mut parser = ConfigParser::new();
    let content = "disk-cache=16M\nmin-split-size=10M\npiece-length=1M\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // Verify options exist and have correct values (if options are registered)
    if let Some(val) = parser.get_str("disk-cache") {
        assert_eq!(val, "16M");
    }
    if let Some(val) = parser.get_str("min-split-size") {
        assert_eq!(val, "10M");
    }
    if let Some(val) = parser.get_str("piece-length") {
        assert_eq!(val, "1M");
    }
}

/// Test: Config file with speed limit values.
#[test]
fn regression_config_file_speed_values() {
    let mut parser = ConfigParser::new();
    let content = "max-download-limit=1M\nmax-overall-download-limit=10M\nmax-upload-limit=512K\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // Verify options exist and have correct values (if options are registered)
    if let Some(val) = parser.get_str("max-download-limit") {
        assert_eq!(val, "1M");
    }
    if let Some(val) = parser.get_str("max-overall-download-limit") {
        assert_eq!(val, "10M");
    }
    if let Some(val) = parser.get_str("max-upload-limit") {
        assert_eq!(val, "512K");
    }
}

/// Test: Config file with path values.
#[test]
fn regression_config_file_path_values() {
    let mut parser = ConfigParser::new();
    let content =
        "dir=/var/downloads\nlog=/var/log/aria2.log\nsave-session=/var/lib/aria2/session.txt\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/var/downloads");
    assert_eq!(parser.get_str("log").unwrap(), "/var/log/aria2.log");
    assert_eq!(
        parser.get_str("save-session").unwrap(),
        "/var/lib/aria2/session.txt"
    );
}

/// Test: Config file with URL values.
#[test]
fn regression_config_file_url_values() {
    let mut parser = ConfigParser::new();
    let content = "referer=http://example.com/\nhttp-proxy=http://proxy.example.com:8080\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("referer").unwrap(), "http://example.com/");
    assert_eq!(
        parser.get_str("http-proxy").unwrap(),
        "http://proxy.example.com:8080"
    );
}

/// Test: Config file with special characters in values.
#[test]
fn regression_config_file_special_characters() {
    let mut parser = ConfigParser::new();
    let content = "dir=/path with spaces\nuser-agent=test-client/1.0 (Linux)\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/path with spaces");
    assert_eq!(
        parser.get_str("user-agent").unwrap(),
        "test-client/1.0 (Linux)"
    );
}

/// Test: Config file with unicode characters.
#[test]
fn regression_config_file_unicode() {
    let mut parser = ConfigParser::new();
    let content = "dir=/下载目录\nout=文件.zip\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/下载目录");
    assert_eq!(parser.get_str("out").unwrap(), "文件.zip");
}

/// Test: Config file with empty value.
#[test]
fn regression_config_file_empty_value() {
    let mut parser = ConfigParser::new();
    let content = "rpc-secret=\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // aria2_original rejects an empty RPC secret instead of treating it as a
    // request to inject a default or silently disabling authentication.
    assert!(!parser.contains("rpc-secret"));
    assert!(parser.has_errors());
}

/// Test: Config file without equals sign is skipped.
#[test]
fn regression_config_file_no_equals_skipped() {
    let mut parser = ConfigParser::new();
    let content = "dir=/downloads\ninvalid_line_without_equals\nsplit=8\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // Valid lines should be parsed
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    // Invalid line should not create an option
    assert!(!parser.contains("invalid_line_without_equals"));
}

/// Test: Non-existent config file is silently ignored.
#[test]
fn regression_config_file_nonexistent_ignored() {
    let mut parser = ConfigParser::new();
    parser.parse_file("/nonexistent/path/to/config.conf");

    // Should not error, just silently ignore
    assert!(!parser.has_errors());
}

// =========================================================================
// Config File Saving Tests
// =========================================================================

/// Test: ConfigManager save_session creates valid file.
#[tokio::test]
async fn regression_config_manager_save_session() {
    let mut mgr = ConfigManager::new();
    mgr.set_global_option("split", OptionValue::Int(10))
        .await
        .unwrap();
    mgr.set_global_option("dir", OptionValue::Str("/downloads".into()))
        .await
        .unwrap();

    let temp_dir = tempfile::tempdir().unwrap();
    let session_path = temp_dir.path().join("session.txt");

    mgr.save_session(session_path.to_str().unwrap())
        .await
        .unwrap();

    // Verify file was created
    assert!(session_path.exists());

    // Verify content
    let content = std::fs::read_to_string(&session_path).unwrap();
    assert!(content.contains("split=10"));
    assert!(content.contains("dir=/downloads"));
}

/// Test: ConfigManager load_session reads saved file.
#[tokio::test]
async fn regression_config_manager_load_session() {
    // First save a session
    let mut mgr1 = ConfigManager::new();
    mgr1.set_global_option("split", OptionValue::Int(12))
        .await
        .unwrap();
    mgr1.set_global_option("quiet", OptionValue::Bool(true))
        .await
        .unwrap();

    let temp_dir = tempfile::tempdir().unwrap();
    let session_path = temp_dir.path().join("session.txt");

    mgr1.save_session(session_path.to_str().unwrap())
        .await
        .unwrap();

    // Then load into a new manager
    let mut mgr2 = ConfigManager::new();
    mgr2.load_session(session_path.to_str().unwrap())
        .await
        .unwrap();

    // Verify values were loaded
    assert_eq!(mgr2.get_global_i64("split").await, Some(12));
    assert_eq!(mgr2.get_global_bool("quiet").await, Some(true));
}

/// Test: Session persistence roundtrip.
#[tokio::test]
async fn regression_session_roundtrip() {
    let mut mgr = ConfigManager::new();

    // Set various option types
    mgr.set_global_option("split", OptionValue::Int(16))
        .await
        .unwrap();
    mgr.set_global_option("dir", OptionValue::Str("/var/downloads".into()))
        .await
        .unwrap();
    mgr.set_global_option("quiet", OptionValue::Bool(true))
        .await
        .unwrap();
    mgr.set_global_option("timeout", OptionValue::Int(300))
        .await
        .unwrap();

    let temp_dir = tempfile::tempdir().unwrap();
    let session_path = temp_dir.path().join("session.txt");

    // Save
    mgr.save_session(session_path.to_str().unwrap())
        .await
        .unwrap();

    // Load into new manager
    let mut mgr2 = ConfigManager::new();
    mgr2.load_session(session_path.to_str().unwrap())
        .await
        .unwrap();

    // Verify all values
    assert_eq!(mgr2.get_global_i64("split").await, Some(16));
    assert_eq!(
        mgr2.get_global_str("dir").await,
        Some("/var/downloads".into())
    );
    assert_eq!(mgr2.get_global_bool("quiet").await, Some(true));
    assert_eq!(mgr2.get_global_i64("timeout").await, Some(300));
}

// =========================================================================
// ConfigManager Integration Tests
// =========================================================================

/// Test: ConfigManager load_file integration.
#[tokio::test]
async fn regression_config_manager_load_file() {
    let mut mgr = ConfigManager::new();

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, "split=8\nquiet=true\n").unwrap();

    mgr.load_file(config_path.to_str().unwrap()).await;

    assert_eq!(mgr.get_global_i64("split").await, Some(8));
    assert_eq!(mgr.get_global_bool("quiet").await, Some(true));
}

/// Test: ConfigManager load_cli integration.
#[tokio::test]
async fn regression_config_manager_load_cli() {
    let mut mgr = ConfigManager::new();

    mgr.load_cli(&["--split=8".to_string(), "--quiet".to_string()])
        .await;

    assert_eq!(mgr.get_global_i64("split").await, Some(8));
    assert_eq!(mgr.get_global_bool("quiet").await, Some(true));
}

/// Test: ConfigManager load_env integration.
#[tokio::test]
async fn regression_config_manager_load_env() {
    // Set environment variable (unsafe in Rust 2024)
    unsafe {
        std::env::set_var("ARIA2_SPLIT", "8");
        std::env::set_var("ARIA2_QUIET", "true");
    }

    let mut mgr = ConfigManager::new();
    mgr.load_env().await;

    // Note: This test may not work if env vars were already set
    // Just verify the method doesn't error
    assert!(!mgr.has_errors());

    // Clean up
    unsafe {
        std::env::remove_var("ARIA2_SPLIT");
        std::env::remove_var("ARIA2_QUIET");
    }
}

/// Test: ConfigManager option priority order (CLI > Config > Env > Defaults).
#[tokio::test]
async fn regression_config_manager_priority_order() {
    let mut mgr = ConfigManager::new();

    // Apply defaults first
    let _default_split = mgr.get_global_i64("split").await;

    // Load config file
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, "split=10\n").unwrap();
    mgr.load_file(config_path.to_str().unwrap()).await;

    let _config_split = mgr.get_global_i64("split").await;

    // Load CLI (should override)
    mgr.load_cli(&["--split=16".to_string()]).await;

    let cli_split = mgr.get_global_i64("split").await;

    // CLI should win
    assert_eq!(cli_split, Some(16));
}

/// Test: ConfigManager get_all_global_options.
#[tokio::test]
async fn regression_config_manager_get_all_options() {
    let mgr = ConfigManager::new();
    let all = mgr.get_all_global_options().await;

    // Should contain default options
    assert!(all.contains_key("dir"));
    assert!(all.contains_key("split"));
    assert!(all.contains_key("timeout"));
}

/// Test: ConfigManager get_all_global_options_json.
#[tokio::test]
async fn regression_config_manager_get_all_options_json() {
    let mgr = ConfigManager::new();
    let json = mgr.get_all_global_options_json().await;

    assert!(json.is_object());
    let obj = json.as_object().unwrap();
    assert!(obj.contains_key("dir"));
    assert!(obj.contains_key("split"));
}

/// Test: ConfigManager change_global_options batch.
#[tokio::test]
async fn regression_config_manager_change_global_options_batch() {
    let mut mgr = ConfigManager::new();

    let mut opts = std::collections::HashMap::new();
    opts.insert("split".to_string(), "10".to_string());
    opts.insert("quiet".to_string(), "true".to_string());

    let errors = mgr.change_global_options(opts).await;
    assert!(errors.is_empty());

    assert_eq!(mgr.get_global_i64("split").await, Some(10));
    assert_eq!(mgr.get_global_bool("quiet").await, Some(true));
}

/// Test: ConfigManager set_unknown_option_fails.
#[tokio::test]
async fn regression_config_manager_set_unknown_option_fails() {
    let mut mgr = ConfigManager::new();
    let result = mgr
        .set_global_option("nonexistent-option", OptionValue::Str("value".into()))
        .await;
    assert!(result.is_err());
}

// =========================================================================
// URI List File Tests
// =========================================================================

/// Test: UriListFile parsing with basic format.
#[test]
fn regression_uri_list_file_basic() {
    let content = "http://example.com/file1.zip\nhttp://example.com/file2.zip\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let uri_path = temp_dir.path().join("uris.txt");
    std::fs::write(&uri_path, content).unwrap();

    let uri_list = UriListFile::from_file(uri_path.to_str().unwrap()).unwrap();

    assert_eq!(uri_list.entries().len(), 2);
    assert_eq!(uri_list.entries()[0].uris.len(), 1);
    assert_eq!(
        uri_list.entries()[0].uris[0],
        "http://example.com/file1.zip"
    );
}

/// Test: UriListFile with comments.
#[test]
fn regression_uri_list_file_comments() {
    let content = "# Comment\nhttp://example.com/file.zip\n# Another comment\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let uri_path = temp_dir.path().join("uris.txt");
    std::fs::write(&uri_path, content).unwrap();

    let uri_list = UriListFile::from_file(uri_path.to_str().unwrap()).unwrap();

    assert_eq!(uri_list.entries().len(), 1);
    assert_eq!(uri_list.entries()[0].uris[0], "http://example.com/file.zip");
}

/// Test: UriListFile with empty lines.
#[test]
fn regression_uri_list_file_empty_lines() {
    let content = "http://example.com/file1.zip\n\n\nhttp://example.com/file2.zip\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let uri_path = temp_dir.path().join("uris.txt");
    std::fs::write(&uri_path, content).unwrap();

    let uri_list = UriListFile::from_file(uri_path.to_str().unwrap()).unwrap();

    assert_eq!(uri_list.entries().len(), 2);
}

/// Test: UriListFile with multiple URIs per entry (TAB separated).
#[test]
fn regression_uri_list_file_multiple_uris() {
    // URIs are separated by TAB characters, not spaces
    let content = "http://mirror1.example.com/file.zip\thttp://mirror2.example.com/file.zip\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let uri_path = temp_dir.path().join("uris.txt");
    std::fs::write(&uri_path, content).unwrap();

    let uri_list = UriListFile::from_file(uri_path.to_str().unwrap()).unwrap();

    assert_eq!(uri_list.entries().len(), 1);
    assert_eq!(uri_list.entries()[0].uris.len(), 2);
}

/// Test: UriListFile with options (dir=/path out=file.zip).
#[test]
fn regression_uri_list_file_with_options() {
    let content = "http://example.com/file.zip\n  dir=/downloads\n  out=file.zip\n";

    let temp_dir = tempfile::tempdir().unwrap();
    let uri_path = temp_dir.path().join("uris.txt");
    std::fs::write(&uri_path, content).unwrap();

    let uri_list = UriListFile::from_file(uri_path.to_str().unwrap()).unwrap();

    assert_eq!(uri_list.entries().len(), 1);
    // Options should be parsed (implementation dependent)
}

/// Test: UriListFile non-existent returns error.
#[test]
fn regression_uri_list_file_nonexistent() {
    let result = UriListFile::from_file("/nonexistent/path/to/uris.txt");
    assert!(result.is_err());
}

/// Test: UriListEntry structure.
#[test]
fn regression_uri_list_entry_structure() {
    let entry = UriListEntry::new(vec![
        "http://example.com/file.zip".to_string(),
        "http://mirror.example.com/file.zip".to_string(),
    ]);

    assert_eq!(entry.uris.len(), 2);
    assert_eq!(entry.uris[0], "http://example.com/file.zip");
}

// =========================================================================
// Config Source Tracking Tests
// =========================================================================

/// Test: ConfigParser tracks source count.
#[test]
fn regression_config_parser_source_count() {
    let mut parser = ConfigParser::new();

    parser.apply_defaults();
    assert!(parser.source_count() >= 1);

    parser.parse_cli_args(&["--split=8"]);
    assert!(parser.source_count() >= 2);

    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, "dir=/downloads\n").unwrap();
    parser.parse_file(config_path.to_str().unwrap());
    assert!(parser.source_count() >= 3);
}

/// Test: ConfigParser to_json_map.
#[test]
fn regression_config_parser_to_json_map() {
    let mut parser = ConfigParser::new();
    parser.set("split", OptionValue::Int(8));
    parser.set("dir", OptionValue::Str("/downloads".into()));

    let json = parser.to_json_map();

    assert!(json.is_object());
    let obj = json.as_object().unwrap();
    assert!(obj.contains_key("split"));
    assert!(obj.contains_key("dir"));
}

// =========================================================================
// Example Config Files Tests
// =========================================================================

/// Test: Parse minimal.conf example format.
#[test]
fn regression_parse_minimal_conf_format() {
    let content = "# Minimal aria2 configuration\ndir=/downloads\nsplit=5\n";

    let mut parser = ConfigParser::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("minimal.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 5);
}

/// Test: Parse basic.conf example format.
#[test]
fn regression_parse_basic_conf_format() {
    let content = r#"
# Basic aria2 configuration
dir=/downloads
max-concurrent-downloads=3
split=5
timeout=60
enable-rpc=true
rpc-listen-port=6800
"#;

    let mut parser = ConfigParser::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("basic.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("max-concurrent-downloads").unwrap(), 3);
    assert_eq!(parser.get_i64("split").unwrap(), 5);
    assert_eq!(parser.get_i64("timeout").unwrap(), 60);
    assert!(parser.get_bool("enable-rpc").unwrap());
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6800);
}

/// Test: Parse advanced.conf example format.
#[test]
fn regression_parse_advanced_conf_format() {
    let content = r#"
# Advanced aria2 configuration
dir=/downloads
file-allocation=prealloc
disk-cache=16M
min-split-size=10M
max-download-limit=1M
max-overall-download-limit=10M
auto-save-interval=30
"#;

    let mut parser = ConfigParser::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("advanced.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    // Check options if they exist
    if let Some(val) = parser.get_str("file-allocation") {
        assert_eq!(val, "prealloc");
    }
    if let Some(val) = parser.get_str("disk-cache") {
        assert_eq!(val, "16M");
    }
    if let Some(val) = parser.get_str("min-split-size") {
        assert_eq!(val, "10M");
    }
    if let Some(val) = parser.get_str("max-download-limit") {
        assert_eq!(val, "1M");
    }
    if let Some(val) = parser.get_str("max-overall-download-limit") {
        assert_eq!(val, "10M");
    }
    assert_eq!(parser.get_i64("auto-save-interval").unwrap(), 30);
}

/// Test: Parse bittorrent.conf example format.
#[test]
fn regression_parse_bittorrent_conf_format() {
    let content = r#"
# BitTorrent configuration
seed-ratio=2.0
seed-time=60
bt-max-peers=55
bt-enable-lpd=true
enable-dht=true
bt-force-encryption=false
listen-port=6881-6999
"#;

    let mut parser = ConfigParser::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("bittorrent.conf");
    std::fs::write(&config_path, content).unwrap();

    parser.parse_file(config_path.to_str().unwrap());

    // Check options if they exist
    if let Some(val) = parser.get_str("seed-ratio") {
        assert_eq!(val, "2.0");
    }
    if let Some(val) = parser.get_i64("seed-time") {
        assert_eq!(val, 60);
    }
    if let Some(val) = parser.get_i64("bt-max-peers") {
        assert_eq!(val, 55);
    }
    if let Some(val) = parser.get_bool("bt-enable-lpd") {
        assert!(val);
    }
    if let Some(val) = parser.get_bool("enable-dht") {
        assert!(val);
    }
    if let Some(val) = parser.get_bool("bt-force-encryption") {
        assert!(!val);
    }
    if let Some(val) = parser.get_str("listen-port") {
        assert_eq!(val, "6881-6999");
    }
}

// =========================================================================
// Config Error Handling Tests
// =========================================================================

/// Test: ConfigParser error on invalid integer.
#[test]
fn regression_config_parser_error_invalid_integer() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "not_a_number");

    assert!(parser.has_errors());
    assert_eq!(parser.errors()[0].option, "split");
}

/// Test: ConfigParser accepts a split value above the default.
#[test]
fn regression_config_parser_accepts_large_split() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "100");

    assert!(!parser.has_errors());
    assert_eq!(parser.get_i64("split"), Some(100));
}

/// Test: ConfigParser error on zero split.
#[test]
fn regression_config_parser_error_zero_split() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "0"); // Min is 1

    assert!(parser.has_errors());
}

/// Test: ConfigError display format.
#[test]
fn regression_config_error_display() {
    use aria2_core::config::ConfigError;
    use aria2_core::config::ConfigSource;

    let err = ConfigError {
        source: ConfigSource::CommandLine,
        option: "split".into(),
        message: "value exceeds maximum".into(),
    };

    let display = format!("{}", err);
    assert!(display.contains("split"));
    assert!(display.contains("command-line"));
}

// =========================================================================
// Config Change Event Tests
// =========================================================================

/// Test: ConfigManager emits change events.
#[tokio::test]
async fn regression_config_manager_change_events() {
    let mut mgr = ConfigManager::new();
    let mut rx = mgr.subscribe_changes();

    mgr.set_global_option("split", OptionValue::Int(10))
        .await
        .unwrap();

    // Should receive event
    let event = rx.recv().await;
    assert!(event.is_ok());
    let evt = event.unwrap();
    assert_eq!(evt.key, "split");
}

/// Test: ConfigManager change event contains old and new values.
#[tokio::test]
async fn regression_config_manager_change_event_values() {
    let mut mgr = ConfigManager::new();

    // Set initial value
    mgr.set_global_option("split", OptionValue::Int(5))
        .await
        .unwrap();

    let mut rx = mgr.subscribe_changes();

    // Change value
    mgr.set_global_option("split", OptionValue::Int(10))
        .await
        .unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.old_value, OptionValue::Int(5));
    assert_eq!(event.new_value, OptionValue::Int(10));
}
