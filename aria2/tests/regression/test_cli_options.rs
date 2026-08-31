//! CLI options regression tests for aria2-rust.
//!
//! These tests verify that clap-based CLI parsing (`CliArgs::try_parse_from`)
//! correctly parses all options and that short/long forms are preserved.
//! Registry and ConfigParser validation tests remain unchanged.

use std::path::PathBuf;

use aria2::app::cli::{CliArgs, Commands, HelpRequest, render_help};
use aria2_core::config::{ConfigParser, OptionCategory, OptionRegistry, OptionType, OptionValue};

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

/// Test: -e is a Rust additive alias for "enable-rpc".
#[test]
fn regression_rust_alias_short_option_enable_rpc() {
    let cli = parse(&["-e"]);
    assert_eq!(cli.rpc.enable_rpc, Some(true));
}

/// Test: -r is a Rust additive alias for "rpc-listen-port".
#[test]
fn regression_rust_alias_short_option_rpc_port() {
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
    let cli = parse(&["-U", "test-client/1.0"]);
    assert_eq!(cli.http_ftp.user_agent.as_deref(), Some("test-client/1.0"));
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

/// Test: -g is a Rust additive alias for "seed-ratio".
#[test]
fn regression_rust_alias_short_option_seed_ratio() {
    let cli = parse(&["-g", "2.0"]);
    assert_eq!(cli.bittorrent.seed_ratio, Some(2.0));
}

/// Test: -G is a Rust additive alias for "seed-time".
#[test]
fn regression_rust_alias_short_option_seed_time() {
    let cli = parse(&["-G", "60"]);
    assert_eq!(cli.bittorrent.seed_time, Some(60.0));
}

/// Test: -B is a Rust additive alias for "bt-max-peers".
#[test]
fn regression_rust_alias_short_option_bt_max_peers() {
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
// Original CLI Boundary Tests (clap-specific)
// =========================================================================

/// Test: -L is a Rust additive alias for "listen-port".
#[test]
fn regression_rust_alias_short_option_listen_port() {
    let cli = parse(&["-L", "6881"]);
    assert_eq!(cli.bittorrent.listen_port.as_deref(), Some("6881"));
}

/// Test: Rust-only short aliases preserve their long-option targets.
#[test]
fn regression_rust_alias_short_option_bt_force_encryption() {
    let cli = parse(&["-X"]);
    assert_eq!(cli.bittorrent.bt_force_encryption, Some(true));
}

/// Test: Rust-only short alias for rpc-secret preserves its value.
#[test]
fn regression_rust_alias_short_option_rpc_secret() {
    let cli = parse(&["-I", "secret"]);
    assert_eq!(cli.rpc.rpc_secret.as_deref(), Some("secret"));
}

/// Test: -h preserves aria2's optional help argument semantics.
#[test]
fn regression_h_does_not_set_listen_port() {
    let cli = CliArgs::try_parse_from(["aria2c", "-h", "6881"])
        .expect("the optional help argument must not consume a space-separated token");
    assert_eq!(cli.help, Some(HelpRequest::Basic));
    assert_eq!(cli.uris, vec!["6881"]);
    assert!(
        cli.bittorrent.listen_port.is_none(),
        "-h must not set listen-port"
    );
}

/// Test: long help selectors are preserved as filters instead of being
/// collapsed into clap's DisplayHelp action.
#[test]
fn regression_help_selector() {
    let cli = parse(&["--help=#http"]);
    assert_eq!(cli.help, Some(HelpRequest::Filter("#http".to_string())));

    let cli = parse(&["-h=timeout"]);
    assert_eq!(cli.help, Some(HelpRequest::Basic));

    let cli = parse(&["-htimeout"]);
    assert_eq!(cli.help, Some(HelpRequest::Filter("timeout".to_string())));
}

/// Test: process-level help rendering consumes selectors without loading a
/// config file or starting the download engine.
#[test]
fn regression_help_rendering_filters_options() {
    let timeout_help = render_help(&HelpRequest::Filter("timeout".to_string()));
    assert!(timeout_help.contains("Usage: aria2c"));
    assert!(timeout_help.contains("--timeout"));
    assert!(!timeout_help.contains("--dir"));

    let http_help = render_help(&HelpRequest::Filter("#http".to_string()));
    assert!(http_help.contains("--http-proxy"));
    assert!(!http_help.contains("--rpc-listen-port"));
}

#[test]
fn regression_basic_help_includes_copyable_examples() {
    let help = render_help(&HelpRequest::Basic);

    assert!(help.contains("https://example.com/file.zip"));
    assert!(help.contains("C:\\Downloads"));
    assert!(help.contains("--option=true"));
}

#[test]
fn regression_help_shows_registry_defaults_without_changing_cli_merge() {
    let help = render_help(&HelpRequest::Basic);

    assert!(help.contains("--dir") && help.contains("[default: .]"));
    assert!(help.contains("--split") && help.contains("[default: 16]"));
    assert!(help.contains("--quiet") && help.contains("[default: false]"));

    let cli = parse(&["--split=4"]);
    assert_eq!(cli.http_ftp.split, Some(4));
}

#[test]
fn regression_help_shows_enum_values() {
    let help = render_help(&HelpRequest::Filter("file-allocation".to_string()));

    for value in ["none", "prealloc", "falloc", "trunc", "mmap"] {
        assert!(
            help.contains(value),
            "help should list file-allocation value {value}: {help}"
        );
    }
    assert!(help.contains("default: prealloc"));
}

#[test]
fn regression_help_shows_ranges_and_units() {
    let timeout_help = render_help(&HelpRequest::Filter("timeout".to_string()));
    assert!(timeout_help.contains("unit: seconds"));
    assert!(timeout_help.contains("range: >=0"));

    let cache_help = render_help(&HelpRequest::Filter("disk-cache".to_string()));
    assert!(cache_help.contains("unit: bytes"));
    assert!(cache_help.contains("K/M/G/T suffixes"));
}

#[test]
fn regression_basic_and_advanced_help_filters_keep_their_groups() {
    let basic_help = render_help(&HelpRequest::Filter("#basic".to_string()));
    assert!(basic_help.contains("--split"));
    assert!(!basic_help.contains("--rpc-listen-port"));
    assert!(!basic_help.contains("--disk-cache"));

    let advanced_help = render_help(&HelpRequest::Filter("#advanced".to_string()));
    assert!(advanced_help.contains("--disk-cache"));
    assert!(!advanced_help.contains("--split"));
    assert!(!advanced_help.contains("--rpc-listen-port"));
}

#[test]
fn regression_check_config_is_an_optional_boolean_action() {
    let cli = parse(&["--check-config"]);
    assert_eq!(cli.general.check_config, Some(true));

    let cli = parse(&["--check-config=false"]);
    assert_eq!(cli.general.check_config, Some(false));
}

/// Test: original public options added from the registry remain reachable
/// through the CLI, including the original short file selectors.
#[test]
fn regression_original_public_option_entries() {
    let cli = parse(&[
        "--async-dns=false",
        "--async-dns-server=127.0.0.1",
        "--event-poll=select",
        "-S",
        "-T",
        "sample.torrent",
        "-M",
        "sample.meta4",
        "--certificate=client.pem",
        "--private-key=client.key",
        "--min-tls-version=TLSv1.2",
        "--ssh-host-key-md=sha-1=deadbeef",
        "--dht-entry-point6=seed.example:6881",
        "--dht-file-path6=dht6.dat",
        "--metalink-enable-unique-protocol=false",
        "--metalink-base-uri=https://example.test/meta4",
        "--on-download-start=hook-start",
        "--pause-metadata",
        "--show-console-readout=false",
        "--dscp=46",
        "--socket-recv-buffer-size=1M",
        "--max-resume-failure-tries=3",
        "--optimize-concurrent-downloads",
    ]);

    assert_eq!(cli.general.async_dns, Some(false));
    assert_eq!(cli.general.async_dns_server.as_deref(), Some("127.0.0.1"));
    assert_eq!(cli.general.event_poll.as_deref(), Some("select"));
    assert_eq!(cli.general.show_files, Some(true));
    assert_eq!(
        cli.general.torrent_file,
        Some(PathBuf::from("sample.torrent"))
    );
    assert_eq!(
        cli.general.metalink_file,
        Some(PathBuf::from("sample.meta4"))
    );
    assert_eq!(cli.http_ftp.certificate, Some(PathBuf::from("client.pem")));
    assert_eq!(cli.http_ftp.private_key, Some(PathBuf::from("client.key")));
    assert_eq!(
        cli.http_ftp.ssh_host_key_md.as_deref(),
        Some("sha-1=deadbeef")
    );
    assert_eq!(
        cli.bittorrent.dht_entry_point6.as_deref(),
        Some("seed.example:6881")
    );
    assert_eq!(cli.general.metalink_enable_unique_protocol, Some(false));
    assert_eq!(cli.general.on_download_start.as_deref(), Some("hook-start"));
    assert_eq!(cli.advanced.dscp, Some(46));
    assert_eq!(cli.advanced.socket_recv_buffer_size.as_deref(), Some("1M"));
    assert_eq!(cli.advanced.max_resume_failure_tries, Some(3));
    assert_eq!(cli.advanced.optimize_concurrent_downloads, Some(true));
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

/// Test: -v is handled by the CLI version action.
#[test]
fn regression_v_triggers_version() {
    let result = CliArgs::try_parse_from(["aria2c", "-v"]);
    let error = result.expect_err("-v must trigger the CLI version action");
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    let version_output = error.to_string();
    assert!(
        version_output.starts_with(&format!(
            "{} {}",
            aria2::identity::PRODUCT_NAME,
            aria2::identity::PRODUCT_VERSION
        )),
        "--version must use the product version number"
    );
    assert!(
        !version_output.contains("Copyright (C) 2006")
            && !version_output.contains("** Configuration **"),
        "--version must not reintroduce the upstream C++ version report"
    );
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
        Some(Commands::CheckUpdate) => panic!("expected Completions subcommand"),
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
            Some(Commands::CheckUpdate) => {
                panic!("expected Completions subcommand for {}", shell)
            }
        }
    }
}

/// Test: update checks expose an explicit opt-out, interval, and command.
#[test]
fn regression_update_check_options() {
    let cli = parse(&["--update-check=false", "--update-check-interval-days=14"]);
    assert_eq!(cli.general.update_check, Some(false));
    assert_eq!(cli.general.update_check_interval_days, Some(14));

    let cli = parse(&["check-update"]);
    assert!(matches!(cli.command, Some(Commands::CheckUpdate)));
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
        "--rpc-listen-port",
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

/// The project-owned compatibility inventory is the source boundary for
/// registry coverage. `help` and `version` are CLI actions, while the
/// remaining differences must be explicit Rust extensions.
#[cfg(feature = "bittorrent")]
#[test]
fn regression_registry_inventory_matches_compatibility_baseline_and_extensions() {
    use std::collections::BTreeSet;

    const COMPATIBILITY_OPTION_INVENTORY: &str =
        include_str!("../fixtures/compatibility_option_inventory.txt");
    const EXPECTED_RUST_EXTENSIONS: &[&str] = &[
        "bt-enable-web-seed",
        "bt-peer-blocklist",
        "bt-tracker-source",
        "bt-tracker-stopped-timeout",
        "bt-tracker-update-interval",
        "enable-public-trackers",
        "enable-utp",
        "log-backup-count",
        "log-max-files",
        "log-max-size",
        "lpd-listen-port",
        "mmap-threshold",
        "on-bt-download-error",
        "pid-file",
        "rpc-allow-origin",
        "rpc-cors-domain",
        "rpc-listen-address",
        "save-server-stat-interval",
        "secure-falloc",
        "utp-listen-port",
        "update-check",
        "update-check-interval-days",
    ];

    let baseline = COMPATIBILITY_OPTION_INVENTORY
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(OptionRegistry::canonical_name)
        .collect::<BTreeSet<_>>();
    let registry = OptionRegistry::new();
    let registered = registry
        .all()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected_extensions = EXPECTED_RUST_EXTENSIONS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    assert_eq!(baseline.len(), 213, "compatibility inventory changed");
    assert_eq!(
        registered.len(),
        233,
        "all-features registry inventory changed"
    );
    assert_eq!(
        baseline
            .difference(&registered)
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["help", "version"]),
        "only CLI actions may be absent from the registry"
    );
    assert_eq!(
        registered
            .difference(&baseline)
            .copied()
            .collect::<BTreeSet<_>>(),
        expected_extensions,
        "Rust-only registry names must remain explicitly enumerated"
    );
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
        &OptionValue::Int(16)
    );
    assert_eq!(
        registry.get("timeout").unwrap().default_value(),
        &OptionValue::Int(60)
    );
    assert_eq!(
        registry.get("auto-save-interval").unwrap().default_value(),
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

/// The short-option table is a Rust-owned baseline for the public CLI
/// contract. These values are checked in here and must not be inferred from
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
        ("bt-save-metadata", None),
        ("enable-dht", None),
        ("follow-torrent", None),
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

/// Rust aliases remain separate from the original short-option contract.
#[test]
fn regression_rust_alias_short_options_are_not_original_contract() {
    let registry = OptionRegistry::new();
    let aliases = [
        ("enable-rpc", 'e'),
        ("rpc-listen-port", 'r'),
        ("rpc-secret", 'I'),
        ("seed-time", 'G'),
        ("seed-ratio", 'g'),
        ("bt-max-peers", 'B'),
        ("bt-force-encryption", 'X'),
        ("listen-port", 'L'),
    ];

    for (name, short_name) in aliases {
        assert_eq!(
            registry.get(name).unwrap().short_name(),
            Some(short_name),
            "Rust extension alias for {name} must remain explicit"
        );
    }
}

/// Rust additive aliases must use the same typed config parser as long names.
#[test]
fn regression_rust_aliases_parse_through_config_seam() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&[
        "-L",
        "6881-6999",
        "-e",
        "-r",
        "6801",
        "-I",
        "secret",
        "-G",
        "60",
        "-g",
        "2.0",
        "-B",
        "55",
        "-X",
    ]);

    assert!(
        !parser.has_errors(),
        "Rust aliases must parse without errors"
    );
    assert_eq!(
        parser.get_str("listen-port"),
        Some("6881-6999"),
        "-L must target the original long option name"
    );
    assert_eq!(parser.get_bool("enable-rpc"), Some(true));
    assert_eq!(parser.get_i64("rpc-listen-port"), Some(6801));
    assert_eq!(parser.get_str("rpc-secret"), Some("secret"));
    assert_eq!(parser.get("seed-time"), Some(&OptionValue::Float(60.0)));
    assert_eq!(parser.get("seed-ratio"), Some(&OptionValue::Float(2.0)));
    assert_eq!(parser.get_i64("bt-max-peers"), Some(55));
    assert_eq!(parser.get_bool("bt-force-encryption"), Some(true));
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

/// Test: split option validation accepts values above the default.
#[test]
fn regression_split_range_validation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--split=5"]);
    assert_eq!(parser.get_i64("split").unwrap(), 5);

    let mut parser2 = ConfigParser::new();
    parser2.parse_cli_args(&["--split=1"]);
    assert_eq!(parser2.get_i64("split").unwrap(), 1);

    let mut parser3 = ConfigParser::new();
    parser3.parse_cli_args(&["--split=100"]);
    assert_eq!(parser3.get_i64("split").unwrap(), 100);
}

/// Test: auto-save-interval keeps aria2's 0..600 second range.
#[test]
fn regression_auto_save_interval_range_validation() {
    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--auto-save-interval=0"]);
    assert_eq!(parser.get_i64("auto-save-interval"), Some(0));

    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--auto-save-interval=600"]);
    assert_eq!(parser.get_i64("auto-save-interval"), Some(600));

    let mut parser = ConfigParser::new();
    parser.parse_cli_args(&["--auto-save-interval=601"]);
    assert!(parser.has_errors());
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
    assert_eq!(parser.get_i64("split").unwrap(), 16);
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
