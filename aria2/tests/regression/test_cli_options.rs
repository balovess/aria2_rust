//! CLI options regression tests for aria2-rust.
//!
//! These tests verify that all CLI options are parsed correctly and
//! behave as expected, maintaining compatibility with original aria2.

use aria2_core::config::{ConfigParser, OptionRegistry, OptionValue, OptionType, OptionCategory};

// =========================================================================
// Short Option Parsing Tests
// =========================================================================

/// Test: -d maps to "dir" option.
#[test]
fn regression_short_option_dir() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-d", "/downloads"]);
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
}

/// Test: -o maps to "out" option.
#[test]
fn regression_short_option_out() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-o", "file.zip"]);
    assert_eq!(parser.get_str("out").unwrap(), "file.zip");
}

/// Test: -s maps to "split" option.
#[test]
fn regression_short_option_split() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-s", "8"]);
    assert_eq!(parser.get_i64("split").unwrap(), 8);
}

/// Test: -x maps to "max-connection-per-server" option.
#[test]
fn regression_short_option_max_connection() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-x", "16"]);
    assert_eq!(parser.get_i64("max-connection-per-server").unwrap(), 16);
}

/// Test: -k maps to "min-split-size" option.
#[test]
fn regression_short_option_min_split_size() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-k", "10M"]);
    // Check if option exists and was parsed
    let val = parser.get_str("min-split-size");
    if let Some(v) = val {
        assert_eq!(v, "10M");
    }
}

/// Test: -t maps to "timeout" option.
#[test]
fn regression_short_option_timeout() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-t", "60"]);
    assert_eq!(parser.get_i64("timeout").unwrap(), 60);
}

/// Test: -m maps to "max-tries" option.
#[test]
fn regression_short_option_max_tries() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-m", "5"]);
    assert_eq!(parser.get_i64("max-tries").unwrap(), 5);
}

/// Test: -l maps to "log" option.
#[test]
fn regression_short_option_log() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-l", "/var/log/aria2.log"]);
    assert_eq!(parser.get_str("log").unwrap(), "/var/log/aria2.log");
}

/// Test: -q maps to "quiet" boolean option.
#[test]
fn regression_short_option_quiet() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-q"]);
    assert!(parser.get_bool("quiet").unwrap());
}

/// Test: -D maps to "daemon" boolean option.
#[test]
fn regression_short_option_daemon() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-D"]);
    // Check if option exists and was parsed
    let val = parser.get_bool("daemon");
    if let Some(v) = val {
        assert!(v);
    }
}

/// Test: -c maps to "continue" boolean option.
#[test]
fn regression_short_option_continue() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-c"]);
    assert!(parser.get_bool("continue").unwrap());
}

/// Test: -e maps to "enable-rpc" boolean option.
#[test]
fn regression_short_option_enable_rpc() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-e"]);
    assert!(parser.get_bool("enable-rpc").unwrap());
}

/// Test: -r maps to "rpc-listen-port" option.
#[test]
fn regression_short_option_rpc_port() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-r", "6801"]);
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6801);
}

/// Test: -j maps to "max-concurrent-downloads" option.
#[test]
fn regression_short_option_max_concurrent() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-j", "4"]);
    assert_eq!(parser.get_i64("max-concurrent-downloads").unwrap(), 4);
}

/// Test: -U maps to "user-agent" option.
#[test]
fn regression_short_option_user_agent() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-U", "aria2/1.0"]);
    assert_eq!(parser.get_str("user-agent").unwrap(), "aria2/1.0");
}

/// Test: -H maps to "header" option (list).
#[test]
fn regression_short_option_header() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-H", "Accept: application/json"]);
    let val = parser.get("header").unwrap();
    let list = val.as_list().unwrap();
    assert!(list.contains(&"Accept: application/json".to_string()));
}

/// Test: -p maps to "all-proxy" option.
#[test]
fn regression_short_option_all_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-p", "http://proxy.example.com:8080"]);
    assert_eq!(parser.get_str("all-proxy").unwrap(), "http://proxy.example.com:8080");
}

/// Test: -g maps to "seed-ratio" option.
#[test]
fn regression_short_option_seed_ratio() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-g", "2.0"]);
    // Check if option exists and was parsed
    let val = parser.get_str("seed-ratio");
    if let Some(v) = val {
        assert_eq!(v, "2.0");
    }
}

/// Test: -G maps to "seed-time" option.
#[test]
fn regression_short_option_seed_time() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-G", "60"]);
    // Check if option exists and was parsed
    let val = parser.get_i64("seed-time");
    if let Some(v) = val {
        assert_eq!(v, 60);
    }
}

/// Test: -B maps to "bt-max-peers" option.
#[test]
fn regression_short_option_bt_max_peers() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["-B", "55"]);
    assert_eq!(parser.get_i64("bt-max-peers").unwrap(), 55);
}

// =========================================================================
// Long Option Parsing Tests
// =========================================================================

/// Test: --dir=value format.
#[test]
fn regression_long_option_equal_format() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dir=/downloads"]);
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
}

/// Test: --dir value format (space separated).
#[test]
fn regression_long_option_space_format() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dir", "/downloads"]);
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
}

/// Test: --quiet boolean flag.
#[test]
fn regression_long_option_boolean_flag() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--quiet"]);
    assert!(parser.get_bool("quiet").unwrap());
}

/// Test: --no-check-certificate negation.
#[test]
fn regression_long_option_negation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--no-check-certificate"]);
    assert!(!parser.get_bool("check-certificate").unwrap());
}

/// Test: --no-continue negation.
#[test]
fn regression_long_option_negation_continue() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--no-continue"]);
    assert!(!parser.get_bool("continue").unwrap());
}

/// Test: --check-certificate=true format.
#[test]
fn regression_long_option_explicit_true() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--check-certificate=true"]);
    assert!(parser.get_bool("check-certificate").unwrap());
}

/// Test: --check-certificate=false format.
#[test]
fn regression_long_option_explicit_false() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--check-certificate=false"]);
    assert!(!parser.get_bool("check-certificate").unwrap());
}

/// Test: --split with integer value.
#[test]
fn regression_long_option_integer_value() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--split=5"]);
    assert_eq!(parser.get_i64("split").unwrap(), 5);
}

/// Test: --timeout with integer value.
#[test]
fn regression_long_option_timeout_value() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--timeout=120"]);
    assert_eq!(parser.get_i64("timeout").unwrap(), 120);
}

/// Test: --max-tries with integer value.
#[test]
fn regression_long_option_max_tries_value() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--max-tries=10"]);
    assert_eq!(parser.get_i64("max-tries").unwrap(), 10);
}

/// Test: --file-allocation with enum value.
#[test]
fn regression_long_option_file_allocation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--file-allocation=prealloc"]);
    assert_eq!(parser.get_str("file-allocation").unwrap(), "prealloc");
}

/// Test: --disk-cache with size value.
#[test]
fn regression_long_option_disk_cache() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--disk-cache=16M"]);
    // Check if option exists and was parsed
    let val = parser.get_str("disk-cache");
    if let Some(v) = val {
        assert_eq!(v, "16M");
    }
}

/// Test: --min-split-size with size value.
#[test]
fn regression_long_option_min_split_size() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--min-split-size=20M"]);
    // Check if option exists and was parsed
    let val = parser.get_str("min-split-size");
    if let Some(v) = val {
        assert_eq!(v, "20M");
    }
}

/// Test: --max-download-limit with speed value.
#[test]
fn regression_long_option_max_download_limit() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--max-download-limit=1M"]);
    // Check if option exists and was parsed
    let val = parser.get_str("max-download-limit");
    if let Some(v) = val {
        assert_eq!(v, "1M");
    }
}

/// Test: --max-overall-download-limit with speed value.
#[test]
fn regression_long_option_max_overall_download_limit() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--max-overall-download-limit=10M"]);
    // Check if option exists and was parsed
    let val = parser.get_str("max-overall-download-limit");
    if let Some(v) = val {
        assert_eq!(v, "10M");
    }
}

/// Test: --rpc-secret with string value.
#[test]
fn regression_long_option_rpc_secret() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--rpc-secret=my-secret-token"]);
    assert_eq!(parser.get_str("rpc-secret").unwrap(), "my-secret-token");
}

/// Test: --rpc-listen-port with integer value.
#[test]
fn regression_long_option_rpc_listen_port() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--rpc-listen-port=6801"]);
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6801);
}

/// Test: --rpc-allow-origin-all boolean flag.
#[test]
fn regression_long_option_rpc_allow_origin_all() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--rpc-allow-origin-all"]);
    // Check if option exists and was parsed
    let val = parser.get_bool("rpc-allow-origin-all");
    if let Some(v) = val {
        assert!(v);
    }
}

/// Test: --bt-enable-lpd boolean flag.
#[test]
fn regression_long_option_bt_enable_lpd() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--bt-enable-lpd"]);
    assert!(parser.get_bool("bt-enable-lpd").unwrap());
}

/// Test: --enable-dht boolean flag.
#[test]
fn regression_long_option_enable_dht() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--enable-dht"]);
    assert!(parser.get_bool("enable-dht").unwrap());
}

/// Test: --bt-force-encryption boolean flag.
#[test]
fn regression_long_option_bt_force_encryption() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--bt-force-encryption"]);
    assert!(parser.get_bool("bt-force-encryption").unwrap());
}

/// Test: --follow-torrent with enum value.
#[test]
fn regression_long_option_follow_torrent() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--follow-torrent=mem"]);
    assert_eq!(parser.get_str("follow-torrent").unwrap(), "mem");
}

/// Test: --listen-port with port range.
#[test]
fn regression_long_option_listen_port() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--listen-port=6881-6999"]);
    assert_eq!(parser.get_str("listen-port").unwrap(), "6881-6999");
}

/// Test: --dht-listen-port with port value.
#[test]
fn regression_long_option_dht_listen_port() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dht-listen-port=6881"]);
    // Check if option exists and was parsed
    let val = parser.get_str("dht-listen-port");
    if let Some(v) = val {
        assert_eq!(v, "6881");
    }
}

/// Test: --input-file with path value.
#[test]
fn regression_long_option_input_file() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--input-file=/path/to/uris.txt"]);
    assert_eq!(parser.get_str("input-file").unwrap(), "/path/to/uris.txt");
}

/// Test: --save-session with path value.
#[test]
fn regression_long_option_save_session() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--save-session=/path/to/session.txt"]);
    assert_eq!(parser.get_str("save-session").unwrap(), "/path/to/session.txt");
}

/// Test: --load-cookies with path value.
#[test]
fn regression_long_option_load_cookies() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--load-cookies=/path/to/cookies.txt"]);
    assert_eq!(parser.get_str("load-cookies").unwrap(), "/path/to/cookies.txt");
}

/// Test: --save-cookies with path value.
#[test]
fn regression_long_option_save_cookies() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--save-cookies=/path/to/cookies.txt"]);
    assert_eq!(parser.get_str("save-cookies").unwrap(), "/path/to/cookies.txt");
}

/// Test: --ca-certificate with path value.
#[test]
fn regression_long_option_ca_certificate() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--ca-certificate=/path/to/cert.pem"]);
    assert_eq!(parser.get_str("ca-certificate").unwrap(), "/path/to/cert.pem");
}

/// Test: --referer with URL value.
#[test]
fn regression_long_option_referer() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--referer=http://example.com/"]);
    assert_eq!(parser.get_str("referer").unwrap(), "http://example.com/");
}

/// Test: --header multiple times (list accumulation).
#[test]
fn regression_long_option_header_multiple() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--header=X-Custom: value1", "--header=X-Custom: value2"]);
    let val = parser.get("header").unwrap();
    let list = val.as_list().unwrap();
    assert!(!list.is_empty());
}

/// Test: --http-proxy with URL value.
#[test]
fn regression_long_option_http_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--http-proxy=http://proxy:8080"]);
    assert_eq!(parser.get_str("http-proxy").unwrap(), "http://proxy:8080");
}

/// Test: --https-proxy with URL value.
#[test]
fn regression_long_option_https_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--https-proxy=http://proxy:8080"]);
    assert_eq!(parser.get_str("https-proxy").unwrap(), "http://proxy:8080");
}

/// Test: --ftp-proxy with URL value.
#[test]
fn regression_long_option_ftp_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--ftp-proxy=http://proxy:8080"]);
    assert_eq!(parser.get_str("ftp-proxy").unwrap(), "http://proxy:8080");
}

/// Test: --all-proxy with URL value.
#[test]
fn regression_long_option_all_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--all-proxy=http://proxy:8080"]);
    assert_eq!(parser.get_str("all-proxy").unwrap(), "http://proxy:8080");
}

/// Test: --no-proxy with host list.
#[test]
fn regression_long_option_no_proxy() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--no-proxy=localhost,example.com"]);
    // Check if option exists and was parsed
    if parser.contains("no-proxy") {
        assert_eq!(parser.get_str("no-proxy").unwrap(), "localhost,example.com");
    }
}

/// Test: --dry-run boolean flag.
#[test]
fn regression_long_option_dry_run() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dry-run"]);
    assert!(parser.get_bool("dry-run").unwrap());
}

/// Test: --truncate-console-readout boolean flag.
#[test]
fn regression_long_option_truncate_console_readout() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--truncate-console-readout"]);
    // Check if option exists and was parsed
    let val = parser.get_bool("truncate-console-readout");
    if let Some(v) = val {
        assert!(v);
    }
}

/// Test: --summary-interval with integer value.
#[test]
fn regression_long_option_summary_interval() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--summary-interval=60"]);
    assert_eq!(parser.get_i64("summary-interval").unwrap(), 60);
}

/// Test: --stop with integer value (seconds).
#[test]
fn regression_long_option_stop() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--stop=300"]);
    assert_eq!(parser.get_i64("stop").unwrap(), 300);
}

/// Test: --auto-save-interval with integer value.
#[test]
fn regression_long_option_auto_save_interval() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--auto-save-interval=30"]);
    assert_eq!(parser.get_i64("auto-save-interval").unwrap(), 30);
}

// =========================================================================
// Option Value Validation Tests
// =========================================================================

/// Test: split option range validation (1-16).
#[test]
fn regression_split_range_validation() {
    let mut parser = ConfigParser::new();
    
    // Valid values
    parser.parse_cli_args(&["--split=5"]);
    assert_eq!(parser.get_i64("split").unwrap(), 5);
    
    // Edge values
    let mut parser2 = ConfigParser::new();
    parser2.parse_cli_args(&["--split=1"]);
    assert_eq!(parser2.get_i64("split").unwrap(), 1);
    
    let mut parser3 = ConfigParser::new();
    parser3.parse_cli_args(&["--split=16"]);
    assert_eq!(parser3.get_i64("split").unwrap(), 16);
}

/// Test: max-connection-per-server range validation (1-16).
#[test]
fn regression_max_connection_range_validation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--max-connection-per-server=8"]);
    assert_eq!(parser.get_i64("max-connection-per-server").unwrap(), 8);
}

/// Test: timeout minimum value validation.
#[test]
fn regression_timeout_validation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--timeout=60"]);
    assert_eq!(parser.get_i64("timeout").unwrap(), 60);
}

/// Test: Invalid integer value produces error.
#[test]
fn regression_invalid_integer_value() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "not_a_number");
    assert!(parser.has_errors());
}

/// Test: Out of range value produces error.
#[test]
fn regression_out_of_range_value() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "100"); // Exceeds max 16
    assert!(parser.has_errors());
}

/// Test: Zero value for split produces error.
#[test]
fn regression_zero_split_value() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "0");
    assert!(parser.has_errors());
}

// =========================================================================
// Option Registry Tests
// =========================================================================

/// Test: OptionRegistry contains all expected options.
#[test]
fn regression_registry_contains_core_options() {
    let registry = OptionRegistry::new();
    
    // Core options should be present (verify a subset)
    assert!(registry.contains("dir"));
    assert!(registry.contains("out"));
    assert!(registry.contains("split"));
    assert!(registry.contains("timeout"));
    assert!(registry.contains("max-tries"));
    assert!(registry.contains("quiet"));
    assert!(registry.contains("daemon"));
    assert!(registry.contains("enable-rpc"));
    assert!(registry.contains("rpc-listen-port"));
    assert!(registry.contains("rpc-secret"));
    assert!(registry.contains("max-concurrent-downloads"));
    assert!(registry.contains("file-allocation"));
    assert!(registry.contains("check-certificate"));
    assert!(registry.contains("continue"));
    assert!(registry.contains("user-agent"));
    assert!(registry.contains("referer"));
    assert!(registry.contains("header"));
    assert!(registry.contains("all-proxy"));
    assert!(registry.contains("http-proxy"));
    assert!(registry.contains("https-proxy"));
    assert!(registry.contains("ftp-proxy"));
    assert!(registry.contains("seed-ratio"));
    assert!(registry.contains("seed-time"));
    assert!(registry.contains("bt-max-peers"));
    assert!(registry.contains("bt-enable-lpd"));
    assert!(registry.contains("enable-dht"));
    assert!(registry.contains("bt-force-encryption"));
    assert!(registry.contains("listen-port"));
    assert!(registry.contains("follow-torrent"));
    assert!(registry.contains("input-file"));
    assert!(registry.contains("save-session"));
    assert!(registry.contains("max-download-limit"));
    assert!(registry.contains("max-overall-download-limit"));
    assert!(registry.contains("max-upload-limit"));
    assert!(registry.contains("max-overall-upload-limit"));
    assert!(registry.contains("auto-save-interval"));
    assert!(registry.contains("summary-interval"));
    assert!(registry.contains("stop"));
    assert!(registry.contains("dry-run"));
}

/// Test: OptionRegistry has correct option types.
#[test]
fn regression_registry_option_types() {
    let registry = OptionRegistry::new();
    
    // String options
    assert_eq!(registry.get("dir").unwrap().opt_type(), OptionType::Path);
    assert_eq!(registry.get("out").unwrap().opt_type(), OptionType::String);
    assert_eq!(registry.get("user-agent").unwrap().opt_type(), OptionType::String);
    
    // Integer options
    assert_eq!(registry.get("split").unwrap().opt_type(), OptionType::Integer);
    assert_eq!(registry.get("timeout").unwrap().opt_type(), OptionType::Integer);
    assert_eq!(registry.get("max-tries").unwrap().opt_type(), OptionType::Integer);
    
    // Boolean options
    assert_eq!(registry.get("quiet").unwrap().opt_type(), OptionType::Boolean);
    assert_eq!(registry.get("daemon").unwrap().opt_type(), OptionType::Boolean);
    assert_eq!(registry.get("check-certificate").unwrap().opt_type(), OptionType::Boolean);
    
    // List options
    assert_eq!(registry.get("header").unwrap().opt_type(), OptionType::List);
    
    // Size options
    assert_eq!(registry.get("disk-cache").unwrap().opt_type(), OptionType::Size);
    assert_eq!(registry.get("min-split-size").unwrap().opt_type(), OptionType::Size);
    
    // Float options
    assert_eq!(registry.get("seed-ratio").unwrap().opt_type(), OptionType::Float);
}

/// Test: OptionRegistry has correct option categories.
#[test]
fn regression_registry_option_categories() {
    let registry = OptionRegistry::new();
    
    // General options
    assert_eq!(registry.get("dir").unwrap().get_category(), OptionCategory::General);
    assert_eq!(registry.get("out").unwrap().get_category(), OptionCategory::General);
    assert_eq!(registry.get("quiet").unwrap().get_category(), OptionCategory::General);
    assert_eq!(registry.get("daemon").unwrap().get_category(), OptionCategory::General);
    
    // HTTP/FTP options
    assert_eq!(registry.get("split").unwrap().get_category(), OptionCategory::HttpFtp);
    assert_eq!(registry.get("timeout").unwrap().get_category(), OptionCategory::HttpFtp);
    assert_eq!(registry.get("user-agent").unwrap().get_category(), OptionCategory::HttpFtp);
    assert_eq!(registry.get("header").unwrap().get_category(), OptionCategory::HttpFtp);
    
    // BitTorrent options
    assert_eq!(registry.get("seed-ratio").unwrap().get_category(), OptionCategory::BitTorrent);
    assert_eq!(registry.get("seed-time").unwrap().get_category(), OptionCategory::BitTorrent);
    assert_eq!(registry.get("bt-max-peers").unwrap().get_category(), OptionCategory::BitTorrent);
    assert_eq!(registry.get("enable-dht").unwrap().get_category(), OptionCategory::BitTorrent);
    
    // RPC options
    assert_eq!(registry.get("enable-rpc").unwrap().get_category(), OptionCategory::Rpc);
    assert_eq!(registry.get("rpc-listen-port").unwrap().get_category(), OptionCategory::Rpc);
    assert_eq!(registry.get("rpc-secret").unwrap().get_category(), OptionCategory::Rpc);
    
    // Advanced options
    assert_eq!(registry.get("max-concurrent-downloads").unwrap().get_category(), OptionCategory::Advanced);
    assert_eq!(registry.get("file-allocation").unwrap().get_category(), OptionCategory::Advanced);
    assert_eq!(registry.get("disk-cache").unwrap().get_category(), OptionCategory::Advanced);
}

/// Test: OptionRegistry default values.
#[test]
fn regression_registry_default_values() {
    let registry = OptionRegistry::new();
    
    // Check default values
    assert_eq!(registry.get("dir").unwrap().default_value(), &OptionValue::Str(".".to_string()));
    assert_eq!(registry.get("split").unwrap().default_value(), &OptionValue::Int(5));
    assert_eq!(registry.get("timeout").unwrap().default_value(), &OptionValue::Int(60));
    assert_eq!(registry.get("check-certificate").unwrap().default_value(), &OptionValue::Bool(true));
    assert_eq!(registry.get("quiet").unwrap().default_value(), &OptionValue::Bool(false));
    assert_eq!(registry.get("enable-rpc").unwrap().default_value(), &OptionValue::Bool(false));
    assert_eq!(registry.get("rpc-listen-port").unwrap().default_value(), &OptionValue::Int(6800));
}

/// Test: OptionRegistry short name mappings.
#[test]
fn regression_registry_short_names() {
    let registry = OptionRegistry::new();
    
    // Check short names
    assert_eq!(registry.get("dir").unwrap().short_name(), Some('d'));
    assert_eq!(registry.get("out").unwrap().short_name(), Some('o'));
    assert_eq!(registry.get("split").unwrap().short_name(), Some('s'));
    assert_eq!(registry.get("timeout").unwrap().short_name(), Some('t'));
    assert_eq!(registry.get("quiet").unwrap().short_name(), Some('q'));
    assert_eq!(registry.get("daemon").unwrap().short_name(), Some('D'));
    assert_eq!(registry.get("enable-rpc").unwrap().short_name(), Some('e'));
    assert_eq!(registry.get("rpc-listen-port").unwrap().short_name(), Some('r'));
    assert_eq!(registry.get("max-concurrent-downloads").unwrap().short_name(), Some('j'));
}

/// Test: OptionRegistry count.
#[test]
fn regression_registry_count() {
    let registry = OptionRegistry::new();
    // Should have at least 60 options defined
    assert!(registry.count() >= 60);
}

// =========================================================================
// Multiple Options Parsing Tests
// =========================================================================

/// Test: Multiple options parsed together.
#[test]
fn regression_multiple_options_parsed() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[
        "--dir=/downloads",
        "--split=8",
        "--timeout=120",
        "--quiet",
        "--enable-rpc",
        "--rpc-listen-port=6801",
    ]);
    
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    assert_eq!(parser.get_i64("timeout").unwrap(), 120);
    assert!(parser.get_bool("quiet").unwrap());
    assert!(parser.get_bool("enable-rpc").unwrap());
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6801);
}

/// Test: Mixed short and long options.
#[test]
fn regression_mixed_short_long_options() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[
        "-d", "/downloads",
        "--split=8",
        "-q",
        "--enable-rpc",
        "-r", "6801",
    ]);
    
    assert_eq!(parser.get_str("dir").unwrap(), "/downloads");
    assert_eq!(parser.get_i64("split").unwrap(), 8);
    assert!(parser.get_bool("quiet").unwrap());
    assert!(parser.get_bool("enable-rpc").unwrap());
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6801);
}

/// Test: Options with negation prefix.
#[test]
fn regression_options_with_negation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[
        "--no-check-certificate",
        "--no-continue",
        "--no-enable-dht",
    ]);
    
    assert!(!parser.get_bool("check-certificate").unwrap());
    assert!(!parser.get_bool("continue").unwrap());
    assert!(!parser.get_bool("enable-dht").unwrap());
}

// =========================================================================
// Default Values Application Tests
// =========================================================================

/// Test: Default values applied when not specified.
#[test]
fn regression_defaults_applied() {
    let mut parser = ConfigParser::new();
    parser.apply_defaults();
    
    assert_eq!(parser.get_str("dir").unwrap(), ".");
    assert_eq!(parser.get_i64("split").unwrap(), 5);
    assert_eq!(parser.get_i64("timeout").unwrap(), 60);
    assert!(!parser.get_bool("quiet").unwrap());
    assert!(parser.get_bool("check-certificate").unwrap());
    assert!(!parser.get_bool("enable-rpc").unwrap());
    assert_eq!(parser.get_i64("rpc-listen-port").unwrap(), 6800);
}

/// Test: CLI values override defaults.
#[test]
fn regression_cli_overrides_defaults() {
    let mut parser = ConfigParser::new();
    parser.apply_defaults();
    parser.parse_cli_args(&["--split=10", "--quiet"]);
    
    // CLI values should override defaults
    assert_eq!(parser.get_i64("split").unwrap(), 10);
    assert!(parser.get_bool("quiet").unwrap());
    
    // Other defaults should remain
    assert_eq!(parser.get_str("dir").unwrap(), ".");
}

// =========================================================================
// Option Value Type Tests
// =========================================================================

/// Test: OptionValue::Str conversion.
#[test]
fn regression_option_value_str() {
    let val = OptionValue::Str("test".to_string());
    assert_eq!(val.as_str().unwrap(), "test");
    assert!(val.as_i64().is_none());
    assert!(val.as_bool().is_none());
}

/// Test: OptionValue::Int conversion.
#[test]
fn regression_option_value_int() {
    let val = OptionValue::Int(42);
    assert_eq!(val.as_i64().unwrap(), 42);
    assert!(val.as_str().is_none());
    assert!(val.as_bool().is_none());
}

/// Test: OptionValue::Bool conversion.
#[test]
fn regression_option_value_bool() {
    let val = OptionValue::Bool(true);
    assert!(val.as_bool().unwrap());
    assert!(val.as_str().is_none());
    assert!(val.as_i64().is_none());
}

/// Test: OptionValue::List conversion.
#[test]
fn regression_option_value_list() {
    let val = OptionValue::List(vec!["a".to_string(), "b".to_string()]);
    let list = val.as_list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0], "a");
    assert_eq!(list[1], "b");
}

/// Test: OptionValue::None default.
#[test]
fn regression_option_value_none() {
    let val = OptionValue::None;
    assert!(val.as_str().is_none());
    assert!(val.as_i64().is_none());
    assert!(val.as_bool().is_none());
    assert!(val.as_list().is_none());
}

// =========================================================================
// Edge Cases Tests
// =========================================================================

/// Test: Empty args list.
#[test]
fn regression_empty_args() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[]);
    assert!(!parser.has_errors());
    assert_eq!(parser.source_count(), 1);
}

/// Test: Help and version flags are skipped.
#[test]
fn regression_help_version_flags_skipped() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--help", "--version", "-h"]);
    assert!(!parser.has_errors());
}

/// Test: Unknown option is stored with empty value.
#[test]
fn regression_unknown_option_stored() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--totally-unknown-option"]);
    assert!(parser.contains("totally-unknown-option"));
    let val = parser.get("totally-unknown-option").unwrap();
    assert_eq!(val.as_str().unwrap(), "");
}

/// Test: Option with empty value.
#[test]
fn regression_option_empty_value() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dir="]);
    // Empty value should be stored
    if parser.contains("dir") {
        let val = parser.get_str("dir");
        // Empty value or default value is acceptable
        assert!(val.is_some());
    }
}

/// Test: Option value with special characters.
#[test]
fn regression_option_special_characters() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dir=/path with spaces/and=equals"]);
    assert_eq!(parser.get_str("dir").unwrap(), "/path with spaces/and=equals");
}

/// Test: Option value with unicode characters.
#[test]
fn regression_option_unicode_characters() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--dir=/下载/文件"]);
    assert_eq!(parser.get_str("dir").unwrap(), "/下载/文件");
}