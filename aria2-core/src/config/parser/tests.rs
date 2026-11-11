use super::*;

#[test]
fn test_config_source_display() {
    assert_eq!(ConfigSource::CommandLine.to_string(), "command-line");
    assert_eq!(ConfigSource::Defaults.to_string(), "defaults");
}

#[test]
fn test_parser_creation() {
    let p = ConfigParser::new();
    assert_eq!(p.source_count(), 0);
    assert!(!p.has_errors());
}

#[test]
fn test_set_and_get() {
    let mut p = ConfigParser::new();
    p.set("dir", OptionValue::Str("/downloads".into()));
    assert_eq!(p.get_str("dir").unwrap(), "/downloads");
    assert!(p.contains("dir"));
    assert!(matches!(
        p.option_source("dir"),
        Some(ConfigSource::CommandLine)
    ));
    assert!(!p.contains("nonexistent"));
}

#[test]
fn test_option_source_tracks_file_and_environment_overrides() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, "timeout=120\n").unwrap();

    let mut parser = ConfigParser::new();
    parser.apply_defaults();
    parser.parse_file(config_path.to_str().unwrap());
    assert_eq!(parser.get_i64("timeout"), Some(120));
    assert!(matches!(
        parser.option_source("timeout"),
        Some(ConfigSource::ConfigFile)
    ));
    parser.set_raw_from("timeout", "180", ConfigSource::Environment);
    assert_eq!(parser.get_i64("timeout"), Some(180));
    assert!(matches!(
        parser.option_source("timeout"),
        Some(ConfigSource::Environment)
    ));
}

#[test]
fn test_set_raw_string() {
    let mut p = ConfigParser::new();
    p.set_raw("split", "8");
    assert_eq!(p.get_i64("split").unwrap(), 8);
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_repeated_index_out_values_preserve_original_newline_wire_form() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--index-out=1=first.iso", "--index-out", "2=second.iso"]);
    assert_eq!(p.get_str("index-out"), Some("1=first.iso\n2=second.iso"));
    assert!(!p.has_errors());
}

#[test]
fn test_set_raw_bool() {
    let mut p = ConfigParser::new();
    p.set_raw("check-certificate", "false");
    assert!(!p.get_bool("check-certificate").unwrap());
}

#[test]
fn test_parse_cli_args_basic() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--dir=/tmp", "--split=3", "--quiet"]);
    assert_eq!(p.get_str("dir").unwrap(), "/tmp");
    assert_eq!(p.get_i64("split").unwrap(), 3);
    assert!(p.get_bool("quiet").unwrap());
}

#[test]
fn test_explicit_empty_cli_values_do_not_use_defaults() {
    let mut p = ConfigParser::new();
    p.apply_defaults();
    p.parse_cli_args(&["--dir=", "--split="]);

    assert_eq!(p.get_str("dir"), Some(""));
    assert!(p.has_errors());
    assert_eq!(p.get_i64("split"), Some(16));
}

#[test]
fn test_missing_value_for_non_boolean_cli_option_is_an_error() {
    let mut p = ConfigParser::new();
    p.apply_defaults();
    p.parse_cli_args(&["--dir", "--quiet"]);

    assert!(p.has_errors());
    assert_eq!(p.get_str("dir"), Some("."));
    assert_eq!(p.get_bool("quiet"), Some(true));
}

#[test]
fn test_config_file_explicit_empty_values_use_the_same_seam() {
    let mut p = ConfigParser::new();
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("aria2.conf");
    std::fs::write(&config_path, "dir=\nquiet=\nsplit=\n").unwrap();

    p.parse_file(config_path.to_str().unwrap());

    assert_eq!(p.get_str("dir"), Some(""));
    assert_eq!(p.get_bool("quiet"), Some(true));
    assert!(p.has_errors());
    assert!(!p.contains("split"));
}

#[test]
fn test_parse_cli_args_no_prefix() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--no-check-certificate", "--no-continue"]);
    assert!(!p.get_bool("check-certificate").unwrap());
    assert!(!p.get_bool("continue").unwrap());
}

#[test]
fn test_registered_no_prefix_names_accept_their_own_values() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&[
        "--no-proxy=localhost,127.0.0.1",
        "--no-file-allocation-limit=1",
        "--no-netrc=true",
        "--no-want-digest-header=true",
    ]);

    assert!(
        !p.has_errors(),
        "registered no-* option errors: {:?}",
        p.errors()
    );
    assert_eq!(
        p.get("no-proxy").and_then(OptionValue::as_list),
        Some(&vec!["localhost".to_string(), "127.0.0.1".to_string()])
    );
    assert_eq!(p.get_i64("no-file-allocation-limit"), Some(1));
    assert_eq!(p.get_bool("no-netrc"), Some(true));
    assert_eq!(p.get_bool("no-want-digest-header"), Some(true));
}

#[test]
fn test_parse_cli_args_space_separated() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--dir", "/opt/downloads", "--out", "file.iso"]);
    assert_eq!(p.get_str("dir").unwrap(), "/opt/downloads");
    assert_eq!(p.get_str("out").unwrap(), "file.iso");
}

#[test]
fn test_parse_cli_boolean_flag() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["-q", "--dry-run"]);
    assert!(p.get_bool("quiet").unwrap());
    assert!(p.get_bool("dry-run").unwrap());
}

#[test]
fn test_apply_defaults() {
    let mut p = ConfigParser::new();
    p.apply_defaults();
    assert_eq!(p.get_str("dir").unwrap(), ".");
    assert_eq!(p.get_i64("split").unwrap(), 16);
    assert!(p.get_bool("enable-color").unwrap());
}

#[test]
fn test_load_order() {
    let mut p = ConfigParser::new();
    p.load_defaults_first();
    p.set("dir", OptionValue::Str("/override".into()));
    assert_eq!(p.get_str("dir").unwrap(), "/override");
}

#[test]
fn test_to_json_map() {
    let mut p = ConfigParser::new();
    p.set("dir", OptionValue::Str("/tmp".into()));
    p.set("split", OptionValue::Int(10));
    let map = p.to_json_map();
    assert!(map.get("dir").is_some());
    assert!(map.get("split").is_some());
}

#[test]
fn test_error_on_invalid_integer() {
    let mut p = ConfigParser::new();
    p.set_raw("split", "not_a_number");
    assert!(p.has_errors());
    assert_eq!(p.errors()[0].option, "split");
}

#[test]
fn test_error_on_out_of_range() {
    let mut p = ConfigParser::new();
    p.set_raw("retry-wait", "601");
    assert!(p.has_errors());
}

#[test]
fn test_error_display() {
    let err = ConfigError {
        source: ConfigSource::CommandLine,
        option: "split".into(),
        message: "value 200 exceeds maximum 128".into(),
    };
    let s = format!("{}", err);
    assert!(s.contains("split"));
    assert!(s.contains("command-line"));
}

#[test]
fn test_default_parser() {
    let p = ConfigParser::default();
    assert_eq!(p.source_count(), 0);
}

// --- Phase 13 / Wave D — Task D6: 10 CLI Option Parsing Tests ---

#[test]
fn test_d6_01_short_option_dir_maps_to_tmp() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["-d", "/tmp"]);
    assert_eq!(p.get_str("dir").unwrap(), "/tmp");
}

#[test]
fn test_d6_02_short_option_split_maps_to_8() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["-s", "8"]);
    assert_eq!(p.get_i64("split").unwrap(), 8);
}

#[test]
fn test_d6_03_long_option_timeout_equals_30() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--timeout=30"]);
    assert_eq!(p.get_i64("timeout").unwrap(), 30);
}

#[test]
fn test_d6_04_boolean_flag_quiet_sets_true() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--quiet"]);
    assert!(p.get_bool("quiet").unwrap());
}

#[test]
fn test_d6_05_list_option_header_parses_correctly() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--header=X:Y,Z:W"]);
    let val = p.get("header").unwrap();
    let list = val.as_list().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0], "X:Y");
    assert_eq!(list[1], "Z:W");
}

#[test]
fn test_d6_06_unknown_option_returns_error() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--totally-nonexistent-option"]);
    assert!(p.contains("totally-nonexistent-option"));
    let val = p.get("totally-nonexistent-option").unwrap();
    assert_eq!(val.as_str().unwrap(), "");
}

#[test]
fn test_d6_07_out_of_range_value_rejected() {
    let mut p = ConfigParser::new();
    p.set_raw("split", "0");
    assert!(p.has_errors());
    assert!(p.errors()[0].option == "split");
}

#[test]
fn test_d6_08_default_values_applied_when_not_specified() {
    let mut p = ConfigParser::new();
    p.apply_defaults();
    assert_eq!(p.get_str("dir").unwrap(), ".");
    assert_eq!(p.get_i64("split").unwrap(), 16);
    assert_eq!(p.get_i64("timeout").unwrap(), 60);
    assert!(!p.get_bool("quiet").unwrap());
}

#[test]
fn test_d6_09_multiple_options_parsed_together() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&[
        "--dir=/downloads",
        "--split=4",
        "--timeout=30",
        "--quiet",
        "--out=output.bin",
    ]);
    assert_eq!(p.get_str("dir").unwrap(), "/downloads");
    assert_eq!(p.get_i64("split").unwrap(), 4);
    assert_eq!(p.get_i64("timeout").unwrap(), 30);
    assert!(p.get_bool("quiet").unwrap());
    assert_eq!(p.get_str("out").unwrap(), "output.bin");
}

#[test]
fn test_d6_10_help_flag_skipped_without_error() {
    let mut p = ConfigParser::new();
    p.parse_cli_args(&["--help", "--version", "-h"]);
    assert_eq!(p.source_count(), 1);
    assert!(!p.has_errors());
}
