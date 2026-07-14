//! CLI options regression tests for aria2-rust.
//!
//! These tests verify that clap-based CLI parsing (`CliArgs::try_parse_from`)
//! correctly parses all options and that short/long forms are preserved.
//! Registry and ConfigParser validation tests remain unchanged.

use std::path::PathBuf;

use aria2::app::cli::{CliArgs, Commands};
use aria2_core::config::{ConfigParser, OptionCategory, OptionRegistry, OptionType, OptionValue};
use clap::Parser;

/// Helper: parse CLI args via clap. Panics on parse error.
fn parse(args: &[&str]) -> CliArgs {
    // First element is the binary name (argv[0]), required by clap.
    let mut full = vec!["aria2c"];
    full.extend_from_slice(args);
    CliArgs::try_parse_from(full).expect("clap parsing should succeed")
}

// =========================================================================
// Short Option Parsing Tests (clap-based)
// =========================================================================

/// Test: -d maps to "dir" option.
#[test]
fn regression_short_option_dir() {
    let cli = parse(&["-d", "/downloads"]);
    assert_eq!(cli.general.dir, Some(PathBuf::from("/downloads")));
}

/// Test: -o maps to "out" option.
#[test]
fn regression_short_option_out() {
    let cli = parse(&["-o", "file.zip"]);
    assert_eq!(cli.general.out.as_deref(), Some("file.zip"));
}

/// Test: -s maps to "split" option.
#[test]
fn regression_short_option_split() {
    let cli = parse(&["-s", "8"]);
    assert_eq!(cli.http_ftp.split, Some(8));
}

/// Test: -x maps to "max-connection-per-server" option.
#[test]
fn regression_short_option_max_connection() {
    let cli = parse(&["-x", "16"]);
    assert_eq!(cli.http_ftp.max_connection_per_server, Some(16));
}

/// Test: -k maps to "min-split-size" option.
#[test]
fn regression_short_option_min_split_size() {
    let cli = parse(&["-k", "10M"]);
    assert_eq!(cli.http_ftp.min_split_size.as_deref(), Some("10M"));
}

/// Test: -t maps to "timeout" option.
#[test]
fn regression_short_option_timeout() {
    let cli = parse(&["-t", "60"]);
    assert_eq!(cli.http_ftp.timeout, Some(60));
}

/// Test: -m maps to "max-tries" option.
#[test]
fn regression_short_option_max_tries() {
    let cli = parse(&["-m", "5"]);
    assert_eq!(cli.http_ftp.max_tries, Some(5));
}

/// Test: -l maps to "log" option.
#[test]
fn regression_short_option_log() {
    let cli = parse(&["-l", "/var/log/aria2.log"]);
    assert_eq!(cli.general.log, Some(PathBuf::from("/var/log/aria2.log")));
}

/// Test: -q maps to "quiet" boolean option.
#[test]
fn regression_short_option_quiet() {
    let cli = parse(&["-q"]);
    assert!(cli.general.quiet);
}

/// Test: -D maps to "daemon" boolean option.
#[test]
fn regression_short_option_daemon() {
    let cli = parse(&["-D"]);
    assert!(cli.general.daemon);
}

/// Test: -c maps to "continue" boolean option.
#[test]
fn regression_short_option_continue() {
    let cli = parse(&["-c"]);
    assert!(cli.http_ftp.continue_dl);
}

/// Test: -e maps to "enable-rpc" boolean option.
#[test]
fn regression_short_option_enable_rpc() {
    let cli = parse(&["-e"]);
    assert!(cli.rpc.enable_rpc);
}

/// Test: -r maps to "rpc-listen-port" option.
#[test]
fn regression_short_option_rpc_port() {
    let cli = parse(&["-r", "6801"]);
    assert_eq!(cli.rpc.rpc_listen_port, Some(6801));
}

/// Test: -j maps to "max-concurrent-downloads" option.
#[test]
fn regression_short_option_max_concurrent() {
    let cli = parse(&["-j", "4"]);
    assert_eq!(cli.advanced.max_concurrent_downloads, Some(4));
}

/// Test: -U maps to "user-agent" option.
#[test]
fn regression_short_option_user_agent() {
    let cli = parse(&["-U", "aria2/1.0"]);
    assert_eq!(cli.http_ftp.user_agent.as_deref(), Some("aria2/1.0"));
}

/// Test: -H maps to "header" option (list, can be repeated).
#[test]
fn regression_short_option_header() {
    let cli = parse(&["-H", "Accept: application/json"]);
    assert_eq!(
        cli.http_ftp.header,
        vec!["Accept: application/json".to_string()]
    );
}

/// Test: -p maps to "all-proxy" option.
#[test]
fn regression_short_option_all_proxy() {
    let cli = parse(&["-p", "http://proxy.example.com:8080"]);
    assert_eq!(
        cli.http_ftp.all_proxy.as_deref(),
        Some("http://proxy.example.com:8080")
    );
}

/// Test: -g maps to "seed-ratio" option (float).
#[test]
fn regression_short_option_seed_ratio() {
    let cli = parse(&["-g", "2.0"]);
    assert_eq!(cli.bittorrent.seed_ratio, Some(2.0));
}

/// Test: -G maps to "seed-time" option (float).
#[test]
fn regression_short_option_seed_time() {
    let cli = parse(&["-G", "60"]);
    assert_eq!(cli.bittorrent.seed_time, Some(60.0));
}

/// Test: -B maps to "bt-max-peers" option.
#[test]
fn regression_short_option_bt_max_peers() {
    let cli = parse(&["-B", "55"]);
    assert_eq!(cli.bittorrent.bt_max_peers, Some(55));
}

// =========================================================================
// Long Option Parsing Tests (clap-based)
// =========================================================================

/// Test: --dir=value format.
#[test]
fn regression_long_option_equal_format() {
    let cli = parse(&["--dir=/downloads"]);
    assert_eq!(cli.general.dir, Some(PathBuf::from("/downloads")));
}

/// Test: --dir value format (space separated).
#[test]
fn regression_long_option_space_format() {
    let cli = parse(&["--dir", "/downloads"]);
    assert_eq!(cli.general.dir, Some(PathBuf::from("/downloads")));
}

/// Test: --quiet boolean flag.
#[test]
fn regression_long_option_boolean_flag() {
    let cli = parse(&["--quiet"]);
    assert!(cli.general.quiet);
}

/// Test: --no-check-certificate negation flag.
#[test]
fn regression_long_option_negation() {
    let cli = parse(&["--no-check-certificate"]);
    assert!(cli.http_ftp.no_check_certificate);
    // The positive flag should not be set
    assert!(!cli.http_ftp.check_certificate);
}

/// Test: --no-continue negation flag.
#[test]
fn regression_long_option_negation_continue() {
    let cli = parse(&["--no-continue"]);
    assert!(cli.http_ftp.no_continue);
    assert!(!cli.http_ftp.continue_dl);
}

/// Test: --check-certificate flag (positive).
#[test]
fn regression_long_option_check_certificate_positive() {
    let cli = parse(&["--check-certificate"]);
    assert!(cli.http_ftp.check_certificate);
}

/// Test: --split with integer value.
#[test]
fn regression_long_option_integer_value() {
    let cli = parse(&["--split=5"]);
    assert_eq!(cli.http_ftp.split, Some(5));
}

/// Test: --timeout with integer value.
#[test]
fn regression_long_option_timeout_value() {
    let cli = parse(&["--timeout=120"]);
    assert_eq!(cli.http_ftp.timeout, Some(120));
}

/// Test: --max-tries with integer value.
#[test]
fn regression_long_option_max_tries_value() {
    let cli = parse(&["--max-tries=10"]);
    assert_eq!(cli.http_ftp.max_tries, Some(10));
}

/// Test: --file-allocation with enum value.
#[test]
fn regression_long_option_file_allocation() {
    let cli = parse(&["--file-allocation=prealloc"]);
    assert_eq!(cli.advanced.file_allocation.as_deref(), Some("prealloc"));
}

/// Test: --disk-cache with size value.
#[test]
fn regression_long_option_disk_cache() {
    let cli = parse(&["--disk-cache=16M"]);
    assert_eq!(cli.advanced.disk_cache.as_deref(), Some("16M"));
}

/// Test: --min-split-size with size value.
#[test]
fn regression_long_option_min_split_size() {
    let cli = parse(&["--min-split-size=20M"]);
    assert_eq!(cli.http_ftp.min_split_size.as_deref(), Some("20M"));
}

/// Test: --max-download-limit with speed value.
#[test]
fn regression_long_option_max_download_limit() {
    let cli = parse(&["--max-download-limit=1M"]);
    assert_eq!(cli.advanced.max_download_limit.as_deref(), Some("1M"));
}

/// Test: --max-overall-download-limit with speed value.
#[test]
fn regression_long_option_max_overall_download_limit() {
    let cli = parse(&["--max-overall-download-limit=10M"]);
    assert_eq!(
        cli.advanced.max_overall_download_limit.as_deref(),
        Some("10M")
    );
}

/// Test: --rpc-secret with string value.
#[test]
fn regression_long_option_rpc_secret() {
    let cli = parse(&["--rpc-secret=my-secret-token"]);
    assert_eq!(cli.rpc.rpc_secret.as_deref(), Some("my-secret-token"));
}

/// Test: --rpc-listen-port with integer value.
#[test]
fn regression_long_option_rpc_listen_port() {
    let cli = parse(&["--rpc-listen-port=6801"]);
    assert_eq!(cli.rpc.rpc_listen_port, Some(6801));
}

/// Test: --bt-enable-lpd boolean flag.
#[test]
fn regression_long_option_bt_enable_lpd() {
    let cli = parse(&["--bt-enable-lpd"]);
    assert!(cli.bittorrent.bt_enable_lpd);
}

/// Test: --enable-dht boolean flag.
#[test]
fn regression_long_option_enable_dht() {
    let cli = parse(&["--enable-dht"]);
    assert!(cli.bittorrent.enable_dht);
}

/// Test: --no-enable-dht negation flag.
#[test]
fn regression_long_option_no_enable_dht() {
    let cli = parse(&["--no-enable-dht"]);
    assert!(cli.bittorrent.no_enable_dht);
    assert!(!cli.bittorrent.enable_dht);
}

/// Test: --bt-force-encryption boolean flag.
#[test]
fn regression_long_option_bt_force_encryption() {
    let cli = parse(&["--bt-force-encryption"]);
    assert!(cli.bittorrent.bt_force_encryption);
}

/// Test: --follow-torrent with enum value.
#[test]
fn regression_long_option_follow_torrent() {
    let cli = parse(&["--follow-torrent=mem"]);
    assert_eq!(cli.bittorrent.follow_torrent.as_deref(), Some("mem"));
}

/// Test: --listen-port with port range.
#[test]
fn regression_long_option_listen_port() {
    let cli = parse(&["--listen-port=6881-6999"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881-6999"));
}

/// Test: --dht-listen-port with port value.
#[test]
fn regression_long_option_dht_listen_port() {
    let cli = parse(&["--dht-listen-port=6881"]);
    assert_eq!(cli.bittorrent.dht_listen_port, Some(6881));
}

/// Test: --input-file with path value.
#[test]
fn regression_long_option_input_file() {
    let cli = parse(&["--input-file=/path/to/uris.txt"]);
    assert_eq!(
        cli.general.input_file,
        Some(PathBuf::from("/path/to/uris.txt"))
    );
}

/// Test: --save-session with path value.
#[test]
fn regression_long_option_save_session() {
    let cli = parse(&["--save-session=/path/to/session.txt"]);
    assert_eq!(
        cli.general.save_session,
        Some(PathBuf::from("/path/to/session.txt"))
    );
}

/// Test: --load-cookies with path value.
#[test]
fn regression_long_option_load_cookies() {
    let cli = parse(&["--load-cookies=/path/to/cookies.txt"]);
    assert_eq!(
        cli.http_ftp.load_cookies,
        Some(PathBuf::from("/path/to/cookies.txt"))
    );
}

/// Test: --save-cookies with path value.
#[test]
fn regression_long_option_save_cookies() {
    let cli = parse(&["--save-cookies=/path/to/cookies.txt"]);
    assert_eq!(
        cli.http_ftp.save_cookies,
        Some(PathBuf::from("/path/to/cookies.txt"))
    );
}

/// Test: --ca-certificate with path value.
#[test]
fn regression_long_option_ca_certificate() {
    let cli = parse(&["--ca-certificate=/path/to/cert.pem"]);
    assert_eq!(
        cli.http_ftp.ca_certificate,
        Some(PathBuf::from("/path/to/cert.pem"))
    );
}

/// Test: --referer with URL value.
#[test]
fn regression_long_option_referer() {
    let cli = parse(&["--referer=http://example.com/"]);
    assert_eq!(cli.http_ftp.referer.as_deref(), Some("http://example.com/"));
}

/// Test: --header multiple times (list accumulation).
#[test]
fn regression_long_option_header_multiple() {
    let cli = parse(&["--header=X-Custom: value1", "--header=X-Custom: value2"]);
    assert_eq!(
        cli.http_ftp.header,
        vec![
            "X-Custom: value1".to_string(),
            "X-Custom: value2".to_string()
        ]
    );
}

/// Test: --http-proxy with URL value.
#[test]
fn regression_long_option_http_proxy() {
    let cli = parse(&["--http-proxy=http://proxy:8080"]);
    assert_eq!(
        cli.http_ftp.http_proxy.as_deref(),
        Some("http://proxy:8080")
    );
}

/// Test: --https-proxy with URL value.
#[test]
fn regression_long_option_https_proxy() {
    let cli = parse(&["--https-proxy=http://proxy:8080"]);
    assert_eq!(
        cli.http_ftp.https_proxy.as_deref(),
        Some("http://proxy:8080")
    );
}

/// Test: --ftp-proxy with URL value.
#[test]
fn regression_long_option_ftp_proxy() {
    let cli = parse(&["--ftp-proxy=http://proxy:8080"]);
    assert_eq!(cli.http_ftp.ftp_proxy.as_deref(), Some("http://proxy:8080"));
}

/// Test: --all-proxy with URL value.
#[test]
fn regression_long_option_all_proxy() {
    let cli = parse(&["--all-proxy=http://proxy:8080"]);
    assert_eq!(cli.http_ftp.all_proxy.as_deref(), Some("http://proxy:8080"));
}

/// Test: --no-proxy with host list.
#[test]
fn regression_long_option_no_proxy() {
    let cli = parse(&["--no-proxy=localhost,example.com"]);
    assert_eq!(
        cli.http_ftp.no_proxy.as_deref(),
        Some("localhost,example.com")
    );
}

/// Test: --dry-run boolean flag.
#[test]
fn regression_long_option_dry_run() {
    let cli = parse(&["--dry-run"]);
    assert!(cli.general.dry_run);
}

/// Test: --summary-interval with integer value.
#[test]
fn regression_long_option_summary_interval() {
    let cli = parse(&["--summary-interval=60"]);
    assert_eq!(cli.general.summary_interval, Some(60));
}

/// Test: --stop with integer value (seconds).
#[test]
fn regression_long_option_stop() {
    let cli = parse(&["--stop=300"]);
    assert_eq!(cli.advanced.stop, Some(300));
}

/// Test: --auto-save-interval with integer value.
#[test]
fn regression_long_option_auto_save_interval() {
    let cli = parse(&["--auto-save-interval=30"]);
    assert_eq!(cli.general.auto_save_interval, Some(30));
}

// =========================================================================
// New CLI Conflict Resolution Tests (clap-specific)
// =========================================================================

/// Test: -L maps to "listen-port" (renamed from -h).
/// This is a key conflict resolution: -h no longer sets listen-port.
#[test]
fn regression_short_option_listen_port_renamed() {
    let cli = parse(&["-L", "6881"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881"));
}

/// Test: -L with port range.
#[test]
fn regression_short_option_listen_port_range() {
    let cli = parse(&["-L", "6881-6999"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881-6999"));
}

/// Test: -h does NOT set listen-port (it triggers help in clap).
/// Verify that -h with a value is rejected (clap treats -h as help, no arg).
#[test]
fn regression_h_does_not_set_listen_port() {
    // -h triggers help in clap; try_parse_from should return Err for -h with
    // an extra positional value, OR succeed as help request.
    // The key assertion: -h never sets listen_port.
    let result = CliArgs::try_parse_from(["aria2c", "-h", "6881"]);
    // clap will either show help (Err with DisplayHelp) or error.
    // Either way, it should NOT produce a CliArgs where listen_port is set.
    if let Ok(cli) = result {
        assert!(
            cli.bittorrent.listen_port.is_none(),
            "-h must not set listen-port"
        );
    }
    // If Err, that's also acceptable (clap exits with help or error).
}

/// Test: --no-color flag exists and is parsed.
#[test]
fn regression_no_color_flag() {
    let cli = parse(&["--no-color"]);
    assert!(cli.no_color);
}

/// Test: --no-color defaults to false when not specified.
#[test]
fn regression_no_color_default_false() {
    let cli = parse(&[]);
    assert!(!cli.no_color);
}

/// Test: -v maps to verbose (not version).
#[test]
fn regression_v_maps_to_verbose() {
    let cli = parse(&["-v"]);
    assert!(cli.verbose);
}

/// Test: -V maps to version (clap exits with version, not listen-port).
/// This verifies -V does NOT set save-cookies.
#[test]
fn regression_v_does_not_set_save_cookies() {
    // -V triggers version output in clap; try_parse_from returns Err
    // with DisplayVersion kind.
    let result = CliArgs::try_parse_from(["aria2c", "-V"]);
    assert!(
        result.is_err(),
        "-V should trigger version output (clap error)"
    );
}

/// Test: completions subcommand is parsed correctly.
#[test]
fn regression_completions_subcommand() {
    let cli = parse(&["completions", "bash"]);
    match cli.command {
        Some(Commands::Completions { shell }) => {
            assert_eq!(shell, clap_complete::Shell::Bash);
        }
        None => panic!("expected Completions subcommand"),
    }
}

/// Test: completions subcommand accepts all supported shells.
#[test]
fn regression_completions_all_shells() {
    for shell in &["bash", "zsh", "fish", "elvish", "powershell"] {
        let cli = parse(&["completions", shell]);
        match cli.command {
            Some(Commands::Completions { .. }) => {}
            None => panic!("expected Completions subcommand for {}", shell),
        }
    }
}

// =========================================================================
// Multiple Options Parsing Tests (clap-based)
// =========================================================================

/// Test: Multiple options parsed together.
#[test]
fn regression_multiple_options_parsed() {
    let cli = parse(&[
        "--dir=/downloads",
        "--split=8",
        "--timeout=120",
        "--quiet",
        "--enable-rpc",
        "--rpc-listen-port=6801",
    ]);

    assert_eq!(cli.general.dir, Some(PathBuf::from("/downloads")));
    assert_eq!(cli.http_ftp.split, Some(8));
    assert_eq!(cli.http_ftp.timeout, Some(120));
    assert!(cli.general.quiet);
    assert!(cli.rpc.enable_rpc);
    assert_eq!(cli.rpc.rpc_listen_port, Some(6801));
}

/// Test: Mixed short and long options.
#[test]
fn regression_mixed_short_long_options() {
    let cli = parse(&[
        "-d",
        "/downloads",
        "--split=8",
        "-q",
        "--enable-rpc",
        "-r",
        "6801",
    ]);

    assert_eq!(cli.general.dir, Some(PathBuf::from("/downloads")));
    assert_eq!(cli.http_ftp.split, Some(8));
    assert!(cli.general.quiet);
    assert!(cli.rpc.enable_rpc);
    assert_eq!(cli.rpc.rpc_listen_port, Some(6801));
}

/// Test: Options with negation prefix.
#[test]
fn regression_options_with_negation() {
    let cli = parse(&["--no-check-certificate", "--no-continue", "--no-enable-dht"]);

    assert!(cli.http_ftp.no_check_certificate);
    assert!(!cli.http_ftp.check_certificate);
    assert!(cli.http_ftp.no_continue);
    assert!(!cli.http_ftp.continue_dl);
    assert!(cli.bittorrent.no_enable_dht);
    assert!(!cli.bittorrent.enable_dht);
}

/// Test: Positional URIs are collected.
#[test]
fn regression_positional_uris() {
    let cli = parse(&[
        "http://example.com/file1.zip",
        "http://example.com/file2.zip",
    ]);
    assert_eq!(cli.uris.len(), 2);
    assert_eq!(cli.uris[0], "http://example.com/file1.zip");
    assert_eq!(cli.uris[1], "http://example.com/file2.zip");
}

/// Test: Options with special characters in values.
#[test]
fn regression_option_special_characters() {
    let cli = parse(&["--dir=/path with spaces/and=equals"]);
    assert_eq!(
        cli.general.dir,
        Some(PathBuf::from("/path with spaces/and=equals"))
    );
}

/// Test: Option value with unicode characters.
#[test]
fn regression_option_unicode_characters() {
    let cli = parse(&["--dir=/下载/文件"]);
    assert_eq!(cli.general.dir, Some(PathBuf::from("/下载/文件")));
}

// =========================================================================
// Option Registry Tests (unchanged - test aria2-core registry)
// =========================================================================

/// Test: OptionRegistry contains all expected options.
#[test]
fn regression_registry_contains_core_options() {
    let registry = OptionRegistry::new();

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

    assert_eq!(registry.get("dir").unwrap().opt_type(), OptionType::Path);
    assert_eq!(registry.get("out").unwrap().opt_type(), OptionType::String);
    assert_eq!(
        registry.get("user-agent").unwrap().opt_type(),
        OptionType::String
    );
    assert_eq!(
        registry.get("split").unwrap().opt_type(),
        OptionType::Integer
    );
    assert_eq!(
        registry.get("timeout").unwrap().opt_type(),
        OptionType::Integer
    );
    assert_eq!(
        registry.get("max-tries").unwrap().opt_type(),
        OptionType::Integer
    );
    assert_eq!(
        registry.get("quiet").unwrap().opt_type(),
        OptionType::Boolean
    );
    assert_eq!(
        registry.get("daemon").unwrap().opt_type(),
        OptionType::Boolean
    );
    assert_eq!(
        registry.get("check-certificate").unwrap().opt_type(),
        OptionType::Boolean
    );
    assert_eq!(registry.get("header").unwrap().opt_type(), OptionType::List);
    assert_eq!(
        registry.get("disk-cache").unwrap().opt_type(),
        OptionType::Size
    );
    assert_eq!(
        registry.get("min-split-size").unwrap().opt_type(),
        OptionType::Size
    );
    assert_eq!(
        registry.get("seed-ratio").unwrap().opt_type(),
        OptionType::Float
    );
}

/// Test: OptionRegistry has correct option categories.
#[test]
fn regression_registry_option_categories() {
    let registry = OptionRegistry::new();

    assert_eq!(
        registry.get("dir").unwrap().get_category(),
        OptionCategory::General
    );
    assert_eq!(
        registry.get("out").unwrap().get_category(),
        OptionCategory::General
    );
    assert_eq!(
        registry.get("quiet").unwrap().get_category(),
        OptionCategory::General
    );
    assert_eq!(
        registry.get("daemon").unwrap().get_category(),
        OptionCategory::General
    );
    assert_eq!(
        registry.get("split").unwrap().get_category(),
        OptionCategory::HttpFtp
    );
    assert_eq!(
        registry.get("timeout").unwrap().get_category(),
        OptionCategory::HttpFtp
    );
    assert_eq!(
        registry.get("user-agent").unwrap().get_category(),
        OptionCategory::HttpFtp
    );
    assert_eq!(
        registry.get("header").unwrap().get_category(),
        OptionCategory::HttpFtp
    );
    assert_eq!(
        registry.get("seed-ratio").unwrap().get_category(),
        OptionCategory::BitTorrent
    );
    assert_eq!(
        registry.get("seed-time").unwrap().get_category(),
        OptionCategory::BitTorrent
    );
    assert_eq!(
        registry.get("bt-max-peers").unwrap().get_category(),
        OptionCategory::BitTorrent
    );
    assert_eq!(
        registry.get("enable-dht").unwrap().get_category(),
        OptionCategory::BitTorrent
    );
    assert_eq!(
        registry.get("enable-rpc").unwrap().get_category(),
        OptionCategory::Rpc
    );
    assert_eq!(
        registry.get("rpc-listen-port").unwrap().get_category(),
        OptionCategory::Rpc
    );
    assert_eq!(
        registry.get("rpc-secret").unwrap().get_category(),
        OptionCategory::Rpc
    );
    assert_eq!(
        registry
            .get("max-concurrent-downloads")
            .unwrap()
            .get_category(),
        OptionCategory::Advanced
    );
    assert_eq!(
        registry.get("file-allocation").unwrap().get_category(),
        OptionCategory::Advanced
    );
    assert_eq!(
        registry.get("disk-cache").unwrap().get_category(),
        OptionCategory::Advanced
    );
}

/// Test: OptionRegistry default values.
#[test]
fn regression_registry_default_values() {
    let registry = OptionRegistry::new();

    assert_eq!(
        registry.get("dir").unwrap().default_value(),
        &OptionValue::Str(".".to_string())
    );
    assert_eq!(
        registry.get("split").unwrap().default_value(),
        &OptionValue::Int(5)
    );
    assert_eq!(
        registry.get("timeout").unwrap().default_value(),
        &OptionValue::Int(60)
    );
    assert_eq!(
        registry.get("check-certificate").unwrap().default_value(),
        &OptionValue::Bool(true)
    );
    assert_eq!(
        registry.get("quiet").unwrap().default_value(),
        &OptionValue::Bool(false)
    );
    assert_eq!(
        registry.get("enable-rpc").unwrap().default_value(),
        &OptionValue::Bool(false)
    );
    assert_eq!(
        registry.get("rpc-listen-port").unwrap().default_value(),
        &OptionValue::Int(6800)
    );
}

/// Test: OptionRegistry short name mappings.
#[test]
fn regression_registry_short_names() {
    let registry = OptionRegistry::new();

    assert_eq!(registry.get("dir").unwrap().short_name(), Some('d'));
    assert_eq!(registry.get("out").unwrap().short_name(), Some('o'));
    assert_eq!(registry.get("split").unwrap().short_name(), Some('s'));
    assert_eq!(registry.get("timeout").unwrap().short_name(), Some('t'));
    assert_eq!(registry.get("quiet").unwrap().short_name(), Some('q'));
    assert_eq!(registry.get("daemon").unwrap().short_name(), Some('D'));
    assert_eq!(registry.get("enable-rpc").unwrap().short_name(), Some('e'));
    assert_eq!(
        registry.get("rpc-listen-port").unwrap().short_name(),
        Some('r')
    );
    assert_eq!(
        registry
            .get("max-concurrent-downloads")
            .unwrap()
            .short_name(),
        Some('j')
    );
}

/// Test: OptionRegistry count.
#[test]
fn regression_registry_count() {
    let registry = OptionRegistry::new();
    assert!(registry.count() >= 60);
}

// =========================================================================
// ConfigParser Validation Tests (unchanged - test ConfigParser directly)
// =========================================================================

/// Test: split option range validation (1-16).
#[test]
fn regression_split_range_validation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--split=5"]);
    assert_eq!(parser.get_i64("split").unwrap(), 5);

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
    parser.set_raw("split", "100");
    assert!(parser.has_errors());
}

/// Test: Zero value for split produces error.
#[test]
fn regression_zero_split_value() {
    let mut parser = ConfigParser::new();
    parser.set_raw("split", "0");
    assert!(parser.has_errors());
}

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

    assert_eq!(parser.get_i64("split").unwrap(), 10);
    assert!(parser.get_bool("quiet").unwrap());
    assert_eq!(parser.get_str("dir").unwrap(), ".");
}

// =========================================================================
// Option Value Type Tests (unchanged)
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

/// Test: Empty args list produces default CliArgs.
#[test]
fn regression_empty_args() {
    let cli = parse(&[]);
    assert!(cli.uris.is_empty());
    assert!(!cli.verbose);
    assert!(!cli.no_color);
    assert!(cli.command.is_none());
}
