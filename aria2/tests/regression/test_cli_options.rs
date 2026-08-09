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
    assert_eq!(cli.general.quiet, Some(true));
}

/// Test: -D maps to "daemon" boolean option.
#[test]
fn regression_short_option_daemon() {
    let cli = parse(&["-D"]);
    assert_eq!(cli.general.daemon, Some(true));
}

/// Test: -c maps to "continue" boolean option.
#[test]
fn regression_short_option_continue() {
    let cli = parse(&["-c"]);
    assert_eq!(cli.http_ftp.continue_dl, Some(true));
}

/// Test: -e maps to "enable-rpc" boolean option.
#[test]
fn regression_short_option_enable_rpc() {
    let cli = parse(&["-e"]);
    assert_eq!(cli.rpc.enable_rpc, Some(true));
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

/// Test: --header maps to "header" option (list, can be repeated).
#[test]
fn regression_short_option_header() {
    let cli = parse(&["--header", "Accept: application/json"]);
    assert_eq!(
        cli.http_ftp.header,
        vec!["Accept: application/json".to_string()]
    );
}

/// Test: -p maps to the original "ftp-pasv" option.
#[test]
fn regression_short_option_ftp_pasv() {
    let cli = parse(&["-p"]);
    assert_eq!(cli.http_ftp.ftp_pasv, Some(true));
}

/// Test: -a maps to the original file-allocation option.
#[test]
fn regression_short_option_file_allocation() {
    let cli = parse(&["-a", "prealloc"]);
    assert_eq!(cli.advanced.file_allocation.as_deref(), Some("prealloc"));
}

/// Test: -P maps to parameterized-uri rather than http-proxy.
#[test]
fn regression_short_option_parameterized_uri() {
    let cli = parse(&["-P"]);
    assert_eq!(cli.general.parameterized_uri, Some(true));
    assert_eq!(cli.http_ftp.http_proxy, None);
}

/// Test: -Z maps to force-sequential.
#[test]
fn regression_short_option_force_sequential() {
    let cli = parse(&["-Z"]);
    assert_eq!(cli.general.force_sequential, Some(true));
}

/// Test: -n maps to no-netrc rather than dry-run.
#[test]
fn regression_short_option_no_netrc() {
    let cli = parse(&["-n"]);
    assert_eq!(cli.general.no_netrc, Some(true));
    assert_eq!(cli.general.dry_run, None);
}

/// Test: -R maps to remote-time rather than referer.
#[test]
fn regression_short_option_remote_time() {
    let cli = parse(&["-R"]);
    assert_eq!(cli.http_ftp.remote_time, Some(true));
    assert_eq!(cli.http_ftp.referer, None);
}

/// Test: -u maps to the original per-torrent upload limit.
#[test]
fn regression_short_option_max_upload_limit() {
    let cli = parse(&["-u", "2M"]);
    assert_eq!(cli.advanced.max_upload_limit.as_deref(), Some("2M"));
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
    assert_eq!(cli.general.quiet, Some(true));
}

/// Test: --no-check-certificate negation flag.
#[test]
fn regression_long_option_negation() {
    let cli = parse(&["--no-check-certificate"]);
    assert_eq!(cli.http_ftp.no_check_certificate, Some(true));
    // The positive flag should not be set
    assert_eq!(cli.http_ftp.check_certificate, None);
}

/// Test: --no-continue negation flag.
#[test]
fn regression_long_option_negation_continue() {
    let cli = parse(&["--no-continue"]);
    assert_eq!(cli.http_ftp.no_continue, Some(true));
    assert_eq!(cli.http_ftp.continue_dl, None);
}

/// Test: --check-certificate flag (positive).
#[test]
fn regression_long_option_check_certificate_positive() {
    let cli = parse(&["--check-certificate"]);
    assert_eq!(cli.http_ftp.check_certificate, Some(true));
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
    assert_eq!(cli.bittorrent.bt_enable_lpd, Some(true));
}

/// Test: --enable-dht boolean flag.
#[test]
fn regression_long_option_enable_dht() {
    let cli = parse(&["--enable-dht"]);
    assert_eq!(cli.bittorrent.enable_dht, Some(true));
}

/// Test: --no-enable-dht negation flag.
#[test]
fn regression_long_option_no_enable_dht() {
    let cli = parse(&["--no-enable-dht"]);
    assert_eq!(cli.bittorrent.no_enable_dht, Some(true));
    assert_eq!(cli.bittorrent.enable_dht, None);
}

/// Test: --bt-force-encryption boolean flag.
#[test]
fn regression_long_option_bt_force_encryption() {
    let cli = parse(&["--bt-force-encryption"]);
    assert_eq!(cli.bittorrent.bt_force_encryption, Some(true));
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
    assert_eq!(cli.bittorrent.dht_listen_port.as_deref(), Some("6881"));
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
    assert_eq!(cli.general.dry_run, Some(true));
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

/// Test: -L is an additional listen-port alias.
#[test]
fn regression_short_option_listen_port_alias() {
    let cli = parse(&["-L", "6881"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881"));
}

/// Test: -L with port range.
#[test]
fn regression_short_option_listen_port_range() {
    let cli = parse(&["-L", "6881-6999"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881-6999"));
}

/// Test: -h does NOT set listen-port (it triggers the original help action).
/// Verify that -h with a value is rejected because help takes no argument.
#[test]
fn regression_h_does_not_set_listen_port() {
    // -h triggers help; try_parse_from should return Err for -h with
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
    assert_eq!(cli.no_color, Some(true));
}

/// Test: --no-color defaults to false when not specified.
#[test]
fn regression_no_color_default_false() {
    let cli = parse(&[]);
    assert_eq!(cli.no_color, None);
}

/// Test: -v is the original aria2 version flag.
#[test]
fn regression_v_triggers_version() {
    let result = CliArgs::try_parse_from(["aria2c", "-v"]);
    let error = result.expect_err("-v must trigger the original version action");
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
}

/// Test: -O/--index-out is repeatable and preserves argument order.
#[test]
fn regression_index_out_is_repeatable() {
    let cli = parse(&["-O", "1=first.iso", "--index-out=2=second.iso"]);
    assert_eq!(
        cli.bittorrent.index_out,
        vec!["1=first.iso", "2=second.iso"]
    );
}

/// Test: -V is the original aria2 check-integrity short option.
#[test]
fn regression_v_uppercase_enables_check_integrity() {
    let cli = parse(&["-V"]);
    assert_eq!(cli.general.check_integrity, Some(true));
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
    assert_eq!(cli.general.quiet, Some(true));
    assert_eq!(cli.rpc.enable_rpc, Some(true));
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
    assert_eq!(cli.general.quiet, Some(true));
    assert_eq!(cli.rpc.enable_rpc, Some(true));
    assert_eq!(cli.rpc.rpc_listen_port, Some(6801));
}

/// Test: Options with negation prefix.
#[test]
fn regression_options_with_negation() {
    let cli = parse(&["--no-check-certificate", "--no-continue", "--no-enable-dht"]);

    assert_eq!(cli.http_ftp.no_check_certificate, Some(true));
    assert_eq!(cli.http_ftp.check_certificate, None);
    assert_eq!(cli.http_ftp.no_continue, Some(true));
    assert_eq!(cli.http_ftp.continue_dl, None);
    assert_eq!(cli.bittorrent.no_enable_dht, Some(true));
    assert_eq!(cli.bittorrent.enable_dht, None);
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
    assert_eq!(
        registry.get("file-allocation").unwrap().default_value(),
        &OptionValue::Str("prealloc".to_string())
    );
}

/// Test: OptionRegistry short name mappings.
#[test]
fn regression_registry_short_names() {
    let registry = OptionRegistry::new();

    assert_eq!(registry.get("dir").unwrap().short_name(), Some('d'));
    assert_eq!(
        registry.get("check-integrity").unwrap().short_name(),
        Some('V')
    );
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

/// The short-option table is an external CLI contract. These values are
/// copied from aria2_original/src/usage_text.h and must not be inferred from
/// the Rust field layout or from a convenient unused character.
#[test]
fn regression_original_short_option_contract() {
    let registry = OptionRegistry::new();
    let expected = [
        ("dir", Some('d')),
        ("out", Some('o')),
        ("log", Some('l')),
        ("daemon", Some('D')),
        ("split", Some('s')),
        ("timeout", Some('t')),
        ("max-tries", Some('m')),
        ("ftp-pasv", Some('p')),
        ("file-allocation", Some('a')),
        ("force-sequential", Some('Z')),
        ("parameterized-uri", Some('P')),
        ("check-integrity", Some('V')),
        ("continue", Some('c')),
        ("user-agent", Some('U')),
        ("no-netrc", Some('n')),
        ("input-file", Some('i')),
        ("max-concurrent-downloads", Some('j')),
        ("show-files", Some('S')),
        ("torrent-file", Some('T')),
        ("metalink-file", Some('M')),
        ("remote-time", Some('R')),
        ("index-out", Some('O')),
        ("max-upload-limit", Some('u')),
        ("max-connection-per-server", Some('x')),
        ("min-split-size", Some('k')),
        ("quiet", Some('q')),
        ("enable-rpc", Some('e')),
        ("rpc-listen-port", Some('r')),
        ("rpc-secret", Some('I')),
        ("summary-interval", None),
        ("log-level", None),
        ("dry-run", None),
        ("all-proxy", None),
        ("http-proxy", None),
        ("https-proxy", None),
        ("ftp-proxy", None),
        ("no-proxy", None),
        ("referer", None),
        ("header", None),
        ("load-cookies", None),
        ("save-cookies", None),
        ("connect-timeout", None),
        ("retry-wait", None),
        ("check-certificate", None),
        ("ca-certificate", None),
        ("allow-overwrite", None),
        ("disk-cache", None),
        ("piece-length", None),
        ("stop", None),
        ("seed-time", Some('G')),
        ("seed-ratio", Some('g')),
        ("bt-max-peers", Some('B')),
        ("bt-save-metadata", None),
        ("bt-force-encryption", Some('X')),
        ("enable-dht", None),
        ("follow-torrent", None),
        ("listen-port", None),
    ];

    for (name, short_name) in expected {
        let actual = registry
            .get(name)
            .unwrap_or_else(|| panic!("registry is missing option {name}"))
            .short_name();
        assert_eq!(
            actual, short_name,
            "short option for {name} diverges from aria2_original"
        );
    }

    let mut owners = std::collections::HashMap::new();
    for definition in registry.all().values() {
        if let Some(short_name) = definition.short_name() {
            let previous = owners.insert(short_name, definition.name());
            assert!(
                previous.is_none(),
                "short option -{short_name} is assigned to both {previous:?} and {}",
                definition.name()
            );
        }
    }
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
    parser.set_raw("retry-wait", "601");
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
    assert_eq!(cli.verbose, None);
    assert_eq!(cli.no_color, None);
    assert!(cli.command.is_none());
}
