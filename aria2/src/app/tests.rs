//! Tests for the App module

use super::cli::CliArgs;
use super::*;
use aria2_core::config::{OptionType, OptionValue};
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::util::rwlock_ext::RwLockRecover;
use clap::CommandFactory;
use std::collections::{BTreeSet, HashMap};
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn redirected_stdout_keeps_plain_console_progress_enabled() {
    assert!(
        console_progress_enabled(true, false),
        "Scoop captures aria2 stdout, but must still receive live progress readout"
    );
}

#[test]
fn console_progress_respects_readout_and_quiet_options() {
    assert!(!console_progress_enabled(false, false));
    assert!(!console_progress_enabled(true, true));
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
async fn spawn_torrent_metadata_server(
    body: Vec<u8>,
) -> (
    String,
    std::sync::Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("metadata server should bind");
    let address = listener
        .local_addr()
        .expect("metadata server should expose an address");
    let request_count = std::sync::Arc::new(AtomicUsize::new(0));
    let request_count_for_task = std::sync::Arc::clone(&request_count);
    let task = tokio::spawn(async move {
        let (mut stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
                .await
                .expect("metadata request timed out")
                .expect("metadata server accept failed");
        request_count_for_task.fetch_add(1, Ordering::Relaxed);

        let mut request = [0u8; 4096];
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut request))
            .await
            .expect("metadata request read timed out")
            .expect("metadata request read failed");
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-bittorrent\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .expect("metadata response headers should be written");
        stream
            .write_all(&body)
            .await
            .expect("metadata response body should be written");
    });

    (
        format!("http://{address}/empty.torrent"),
        request_count,
        task,
    )
}

async fn read_http_response(port: u16, request: &str) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut stream = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => break stream,
            Err(_error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("RPC server did not accept a connection: {error}"),
        }
    };

    stream
        .write_all(request.as_bytes())
        .await
        .expect("write HTTP request");
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_to_end(&mut response),
    )
    .await
    .expect("read HTTP response timed out")
    .expect("read HTTP response");
    String::from_utf8(response).expect("RPC response should be UTF-8")
}

#[tokio::test]
async fn test_cli_metalink_options_reach_download_options() {
    let cli = CliArgs::try_parse_from([
        "aria2",
        "--follow-metalink=mem",
        "--metalink-version=4.0",
        "--metalink-language=en",
        "--metalink-os=linux",
        "--metalink-location=us,jp",
        "--metalink-preferred-protocol=https",
        "--select-file=2",
        "https://example.test/metadata",
    ])
    .expect("Metalink CLI options should parse");

    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("CLI options should load into ConfigManager");
    let (options, _) = app.download_options_with_snapshot().await;

    assert_eq!(
        options.follow_metalink,
        Some(aria2_core::request::request_group::FollowMode::Memory)
    );
    assert_eq!(options.metalink_version.as_deref(), Some("4.0"));
    assert_eq!(options.metalink_language.as_deref(), Some("en"));
    assert_eq!(options.metalink_os.as_deref(), Some("linux"));
    assert_eq!(options.metalink_location.as_deref(), Some("us,jp"));
    assert_eq!(
        options.metalink_preferred_protocol.as_deref(),
        Some("https")
    );
    assert_eq!(options.select_file.as_deref(), Some("2"));
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn test_cli_bittorrent_timeout_and_dht_options_reach_download_options() {
    let cli = CliArgs::try_parse_from([
        "aria2",
        "--bt-keep-alive-interval=31",
        "--bt-timeout=181",
        "--bt-request-timeout=61",
        "--peer-connection-timeout=16",
        "--bt-peer-blocklist=blocked-peers.txt",
        "--enable-dht6=true",
        "--dht-listen-addr=127.0.0.1",
        "--dht-listen-addr6=::1",
        "--dht-entry-point-host=127.0.0.1",
        "--dht-entry-point-port=6881",
        "--dht-entry-point6=[::1]:6882",
        "--dht-entry-point-host6=::1",
        "--dht-entry-point-port6=6882",
        "--dht-file-path6=dht6.dat",
    ])
    .expect("BitTorrent timeout and DHT CLI options should parse");

    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("CLI options should load into ConfigManager");
    let (options, _) = app.download_options_with_snapshot().await;

    assert_eq!(options.bt_keep_alive_interval, 31);
    assert_eq!(options.bt_timeout, 181);
    assert_eq!(options.bt_request_timeout, 61);
    assert_eq!(options.peer_connection_timeout, 16);
    assert_eq!(
        options.bt_peer_blocklist.as_deref(),
        Some("blocked-peers.txt")
    );
    assert!(options.enable_dht6);
    assert_eq!(options.dht_listen_addr.as_deref(), Some("127.0.0.1"));
    assert_eq!(options.dht_listen_addr6.as_deref(), Some("::1"));
    assert_eq!(options.dht_entry_point_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(options.dht_entry_point_port, Some(6881));
    assert_eq!(options.dht_entry_point6.as_deref(), Some("[::1]:6882"));
    assert_eq!(options.dht_entry_point_host6.as_deref(), Some("::1"));
    assert_eq!(options.dht_entry_point_port6, Some(6882));
    assert_eq!(options.dht_file_path6.as_deref(), Some("dht6.dat"));
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn test_bt_input_does_not_inherit_implicit_generic_timeout() {
    let temp_dir = TempDir::new().expect("temporary input directory");
    let torrent_path = temp_dir.path().join("input.torrent");
    tokio::fs::write(&torrent_path, b"d8:announce0:e")
        .await
        .expect("write torrent fixture");

    let cli = CliArgs::try_parse_from([
        "aria2",
        torrent_path.to_str().expect("torrent path is UTF-8"),
    ])
    .expect("torrent CLI should parse");
    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("torrent input should be detected");
    let (options, _) = app.download_options_with_snapshot().await;
    let task_options = super::engine::task_options_for_input(
        &options,
        &aria2_core::validation::protocol_detector::InputType::TorrentFile,
        false,
    );
    assert_eq!(task_options.timeout, None);
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn test_bt_input_preserves_explicit_generic_timeout() {
    let temp_dir = TempDir::new().expect("temporary input directory");
    let torrent_path = temp_dir.path().join("input.torrent");
    tokio::fs::write(&torrent_path, b"d8:announce0:e")
        .await
        .expect("write torrent fixture");

    let cli = CliArgs::try_parse_from([
        "aria2",
        "--timeout=300",
        torrent_path.to_str().expect("torrent path is UTF-8"),
    ])
    .expect("torrent CLI should parse");
    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("torrent input should be detected");
    let (options, _) = app.download_options_with_snapshot().await;
    let task_options = super::engine::task_options_for_input(
        &options,
        &aria2_core::validation::protocol_detector::InputType::TorrentFile,
        true,
    );
    assert_eq!(task_options.timeout, Some(300));
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn test_bt_input_preserves_explicit_generic_timeout_from_config_file() {
    let temp_dir = TempDir::new().expect("temporary input directory");
    let torrent_path = temp_dir.path().join("input.torrent");
    tokio::fs::write(&torrent_path, b"d8:announce0:e")
        .await
        .expect("write torrent fixture");
    let config_path = temp_dir.path().join("aria2.conf");
    tokio::fs::write(&config_path, b"timeout=60\n")
        .await
        .expect("write config fixture");

    let mut app = App::new();
    app.load_startup_config(false, config_path.to_str())
        .await
        .expect("config file should load");
    let cli = CliArgs::try_parse_from([
        "aria2",
        torrent_path.to_str().expect("torrent path is UTF-8"),
    ])
    .expect("torrent CLI should parse");
    app.load_cli_args(cli)
        .await
        .expect("torrent input should be detected");
    assert!(app.explicit_timeout);

    let (options, snapshot) = app.download_options_with_snapshot().await;
    let task_options = super::engine::task_options_for_input(
        &options,
        &aria2_core::validation::protocol_detector::InputType::TorrentFile,
        app.explicit_timeout,
    );
    assert_eq!(task_options.timeout, Some(60));
    assert_eq!(snapshot.get("timeout"), Some(&serde_json::json!(60)));
}

#[test]
fn every_registered_option_has_one_cli_argument() {
    let registry = aria2_core::config::OptionRegistry::new();
    let mut cli_names = BTreeSet::new();
    let mut duplicate_cli_names = BTreeSet::new();

    for argument in CliArgs::command().get_arguments() {
        if let Some(name) = argument.get_long()
            && !cli_names.insert(name.to_string())
        {
            duplicate_cli_names.insert(name.to_string());
        }
    }

    assert!(
        duplicate_cli_names.is_empty(),
        "CLI has duplicate long option names: {duplicate_cli_names:?}"
    );

    let missing = registry
        .all()
        .keys()
        .filter(|name| !cli_names.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "registered options without a CLI argument: {missing:?}"
    );
}

fn cli_contract_value(definition: &aria2_core::config::OptionDef) -> String {
    match definition.opt_type() {
        OptionType::Boolean => "true".to_string(),
        OptionType::Integer | OptionType::IntegerRange => {
            definition.min.unwrap_or(1).max(0).to_string()
        }
        OptionType::Float => "1.5".to_string(),
        OptionType::List => "first,second".to_string(),
        OptionType::Enum => definition
            .allowed_values()
            .first()
            .copied()
            .unwrap_or("contract-value")
            .to_string(),
        OptionType::Ipv4Address => "127.0.0.1".to_string(),
        OptionType::IndexOut => "1=contract-output.bin".to_string(),
        OptionType::PiecePriority => "head=1K".to_string(),
        OptionType::Path | OptionType::String => {
            if definition.name() == "checksum" {
                "sha-256=contract-digest".to_string()
            } else {
                "contract-consumer-value".to_string()
            }
        }
        OptionType::Size => definition.min.unwrap_or(1).max(1).to_string(),
    }
}

#[tokio::test]
async fn every_registered_option_reaches_config_manager_from_cli() {
    let registry = aria2_core::config::OptionRegistry::new();
    let process_only = ["conf-path", "no-conf", "torrent-file", "metalink-file"];

    for definition in registry.all().values() {
        if process_only.contains(&definition.name()) {
            continue;
        }

        if !definition.is_supported() {
            let raw = cli_contract_value(definition);
            let argument = format!("--{}={raw}", definition.name());
            let cli =
                CliArgs::try_parse_from(["aria2", argument.as_str()]).unwrap_or_else(|error| {
                    panic!(
                        "registered option '{}' must remain addressable through the real CLI: {}",
                        definition.name(),
                        error
                    )
                });
            let mut app = App::new();
            let error = app
                .load_cli_args(cli)
                .await
                .expect_err("unsupported options must be rejected by the CLI adapter");
            assert!(
                error.contains(definition.name()),
                "unsupported option '{}' error must identify the option: {}",
                definition.name(),
                error
            );
            continue;
        }

        let raw = cli_contract_value(definition);
        assert!(
            definition.parse_value(&raw).is_ok(),
            "contract value for '{}' must be valid: {:?}",
            definition.name(),
            definition.parse_value(&raw).err()
        );
        let argument = format!("--{}={raw}", definition.name());
        let cli = CliArgs::try_parse_from(["aria2", argument.as_str()]).unwrap_or_else(|error| {
            panic!(
                "registered option '{}' must parse through the real CLI: {}",
                definition.name(),
                error
            )
        });

        let mut app = App::new();
        app.load_cli_args(cli).await.unwrap_or_else(|error| {
            panic!(
                "registered option '{}' must load through the real CLI adapter: {}",
                definition.name(),
                error
            )
        });
        let actual = app
            .config
            .read()
            .await
            .get_global_option(definition.name())
            .await;
        let expected = definition
            .parse_value(&raw)
            .expect("validated contract value");
        assert_eq!(
            actual,
            Some(expected),
            "registered option '{}' must be stored once with its registry type",
            definition.name()
        );
    }
}

#[tokio::test]
async fn test_load_cli_args_rejects_invalid_split() {
    let cli = CliArgs::try_parse_from(["aria2", "--split=0"])
        .expect("clap should parse the integer before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("the registry must reject split=0");

    assert!(error.contains("--split"), "unexpected error: {error}");
}

#[tokio::test]
async fn test_load_cli_args_rejects_invalid_file_allocation() {
    let cli = CliArgs::try_parse_from(["aria2", "--file-allocation=invalid"])
        .expect("clap should parse the string before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("the registry must reject an unknown allocation mode");

    assert!(
        error.contains("--file-allocation"),
        "unexpected error: {error}"
    );
}

#[cfg(feature = "bittorrent")]
#[tokio::test]
async fn test_load_cli_args_rejects_invalid_lpd_interface() {
    let cli = CliArgs::try_parse_from(["aria2", "--bt-lpd-interface=Ethernet"])
        .expect("clap should parse the interface before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("LPD must reject an interface value that cannot be used as an IPv4 address");

    assert!(
        error.contains("--bt-lpd-interface"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_server_stat_configuration_reaches_engine_owner() {
    let temp_dir = tempfile::tempdir().expect("server-stat test directory");
    let input = temp_dir.path().join("server-stat-in.json");
    let output = temp_dir.path().join("server-stat-out.json");
    let app = App::new();

    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "server-stat-if",
                OptionValue::Str(input.to_string_lossy().into_owned()),
            )
            .await
            .expect("server-stat input path should validate");
        config
            .set_global_option(
                "server-stat-file",
                OptionValue::Str(output.to_string_lossy().into_owned()),
            )
            .await
            .expect("server-stat-file alias should validate");
        config
            .set_global_option("server-stat-timeout", OptionValue::Int(0))
            .await
            .expect("zero server-stat timeout should mean unlimited");
        config
            .set_global_option("save-server-stat-interval", OptionValue::Int(17))
            .await
            .expect("server-stat save interval should validate");
    }

    app.initialize_engine().await;
    let engine = app.engine.lock().await;
    let engine = engine.as_ref().expect("engine should be initialized");
    assert_eq!(engine.server_stat_input_path(), Some(&input));
    assert_eq!(engine.server_stat_output_path(), Some(&output));
    assert_eq!(engine.server_stat_max_age(), None);
    assert_eq!(
        engine.server_stat_save_interval(),
        Some(std::time::Duration::from_secs(17))
    );
}

#[tokio::test]
async fn test_load_cli_args_accepts_original_piece_priority_syntax() {
    let cli = CliArgs::try_parse_from(["aria2", "--bt-prioritize-piece=head=512K,tail"])
        .expect("clap should parse the original piece-priority syntax");
    let mut app = App::new();

    app.load_cli_args(cli)
        .await
        .expect("the registry should accept the original piece-priority syntax");
    let (options, _) = app.download_options_with_snapshot().await;

    assert_eq!(options.bt_prioritize_piece, "head=512K,tail");
}

#[tokio::test]
async fn test_load_cli_args_rejects_legacy_piece_priority_mode() {
    let cli = CliArgs::try_parse_from(["aria2", "--bt-prioritize-piece=rarest"])
        .expect("clap should parse the value before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("the registry must reject the old synthetic piece-priority mode");

    assert!(
        error.contains("--bt-prioritize-piece"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn application_rpc_does_not_enable_cors_by_default() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let port = probe
        .local_addr()
        .expect("test listener should expose an address")
        .port();
    drop(probe);

    let app = App::new();
    {
        let mut config = app.config.write().await;
        config
            .set_global_option("enable-rpc", OptionValue::Bool(true))
            .await
            .expect("enable-rpc should be valid");
        config
            .set_global_option("rpc-listen-port", OptionValue::Int(port as i64))
            .await
            .expect("rpc-listen-port should be valid");
    }

    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = app
        .start_rpc_server(
            super::startup::StartupPlan::resolve(super::startup::StartupInputs {
                has_initial_downloads: false,
                has_input_file: false,
                restored_tasks: 0,
                tui: false,
                configured_rpc: true,
                explicit_rpc: None,
            })
            .unwrap(),
            app.request_man.clone(),
            cmd_tx,
        )
        .await
        .expect("RPC server should start");

    let response = read_http_response(
        port,
        &format!(
            "OPTIONS /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://browser.example\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    server.abort();

    assert!(
        !response
            .lines()
            .any(|line| line.eq_ignore_ascii_case("access-control-allow-origin: *")),
        "aria2_original only emits CORS headers after explicit opt-in, response was:\n{response}"
    );
}

#[tokio::test]
async fn application_run_fails_when_rpc_bind_fails() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let port = occupied
        .local_addr()
        .expect("test listener should expose an address")
        .port();

    let port_argument = format!("--rpc-listen-port={port}");
    let cli = CliArgs::try_parse_from([
        "aria2c",
        "--no-conf=true",
        "--enable-rpc=true",
        "--rpc-listen-address=127.0.0.1",
        "--disable-ipv6=true",
        "--quiet=true",
        port_argument.as_str(),
    ])
    .expect("RPC-only CLI arguments should parse");

    let mut app = App::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), app.run(cli))
        .await
        .expect("startup must fail instead of hanging after RPC bind failure");

    assert_eq!(result, 1);
}

#[tokio::test]
async fn application_run_ignores_config_rpc_for_cli_download() {
    let occupied_rpc = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let rpc_port = occupied_rpc
        .local_addr()
        .expect("test listener should expose an address")
        .port();

    let download_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("download listener should bind");
    let download_port = download_listener
        .local_addr()
        .expect("download listener should expose an address")
        .port();
    let download_server = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            download_listener.accept(),
        )
        .await
        .expect("download request should arrive")
        .expect("download listener should accept");
        let mut request = [0u8; 2048];
        let bytes_read = stream
            .read(&mut request)
            .await
            .expect("download request should be readable");
        assert!(bytes_read > 0, "download request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest")
            .await
            .expect("download response should be writable");
    });

    let temp_dir = TempDir::new().expect("temporary config directory");
    let config_path = temp_dir.path().join("aria2.conf");
    let download_uri = format!("http://127.0.0.1:{download_port}/file");
    tokio::fs::write(
        &config_path,
        format!(
            "enable-rpc=true\ndisable-ipv6=true\nrpc-listen-port={rpc_port}\nstop=1\ndir={}\nout=download.bin\nquiet=true\nshow-console-readout=false\nmax-tries=1\nconnect-timeout=1\n",
            temp_dir.path().display()
        ),
    )
    .await
    .expect("test config should be written");
    let config_argument = config_path.to_str().expect("config path is UTF-8");
    let cli = CliArgs::try_parse_from([
        "aria2c",
        "--conf-path",
        config_argument,
        download_uri.as_str(),
    ])
    .expect("download CLI arguments should parse");

    let mut app = App::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), app.run(cli))
        .await
        .expect("download should not hang");
    download_server
        .await
        .expect("download server task should finish");
    assert_eq!(
        result, 0,
        "a CLI download must ignore enable-rpc from the shared config"
    );
}

#[tokio::test]
async fn test_original_cli_options_reach_config_registry() {
    let cli = CliArgs::try_parse_from([
        "aria2",
        "--async-dns=false",
        "--certificate=client.pem",
        "--private-key=client.key",
        "--min-tls-version=TLSv1.2",
        "--ssh-host-key-md=sha-1=deadbeef",
        "--metalink-enable-unique-protocol=false",
        "--pause-metadata",
        "--show-console-readout=false",
        "--max-resume-failure-tries=3",
    ])
    .expect("new original CLI options should parse");

    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("new original CLI options should use registry validation");

    assert_eq!(app.get_opt_bool("async-dns").await, Some(false));
    assert_eq!(
        app.get_opt_str("certificate").await.as_deref(),
        Some("client.pem")
    );
    assert_eq!(
        app.get_opt_str("ssh-host-key-md").await.as_deref(),
        Some("sha-1=deadbeef")
    );
    assert_eq!(
        app.get_opt_bool("metalink-enable-unique-protocol").await,
        Some(false)
    );
    assert_eq!(app.get_opt_bool("pause-metadata").await, Some(true));
    assert_eq!(app.get_opt_bool("show-console-readout").await, Some(false));
    assert_eq!(app.get_opt_i64("max-resume-failure-tries").await, Some(3));
}

#[tokio::test]
async fn test_torrent_and_metalink_file_options_enter_input_detection() {
    let temp_dir = TempDir::new().expect("temporary input directory");
    let torrent_path = temp_dir.path().join("input.torrent");
    let metalink_path = temp_dir.path().join("input.meta4");
    tokio::fs::write(&torrent_path, b"d8:announce0:e")
        .await
        .expect("write torrent fixture");
    tokio::fs::write(
        &metalink_path,
        br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"></metalink>"#,
    )
    .await
    .expect("write metalink fixture");

    let cli = CliArgs::try_parse_from([
        "aria2",
        "--torrent-file",
        torrent_path.to_str().expect("torrent path is UTF-8"),
        "--metalink-file",
        metalink_path.to_str().expect("metalink path is UTF-8"),
    ])
    .expect("metadata file options should parse");

    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("metadata file options should be detected");

    assert_eq!(app.detected_inputs.len(), 2);
    assert_eq!(
        app.detected_inputs[0].input_type,
        aria2_core::validation::protocol_detector::InputType::TorrentFile
    );
    assert_eq!(
        app.detected_inputs[1].input_type,
        aria2_core::validation::protocol_detector::InputType::MetalinkFile
    );
}

#[tokio::test]
async fn test_input_file_session_is_not_added_as_a_duplicate_uri_input() {
    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_path = temp_dir.path().join("aria2.session");
    let entry = aria2_core::session::session_entry::SessionEntry::new(
        0x42,
        vec!["https://example.test/file.bin".to_string()],
    );
    tokio::fs::write(&session_path, entry.serialize())
        .await
        .expect("session file should be written");

    let cli = CliArgs::try_parse_from([
        "aria2c",
        "--input-file",
        session_path.to_str().expect("session path is UTF-8"),
    ])
    .expect("session CLI arguments should parse");
    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("session input should load");

    assert!(
        app.detected_inputs.is_empty(),
        "a session file must not also become a new URI download"
    );
    assert_eq!(
        app.restore_session().await.expect("session should restore"),
        1
    );
    assert_eq!(app.request_man.list_groups().len(), 1);
}

#[tokio::test]
async fn test_input_file_uri_list_remains_a_download_input() {
    let temp_dir = TempDir::new().expect("temporary URI list directory");
    let input_path = temp_dir.path().join("urls.txt");
    tokio::fs::write(&input_path, "https://example.test/file.bin\n")
        .await
        .expect("URI list should be written");

    let cli = CliArgs::try_parse_from([
        "aria2c",
        "--input-file",
        input_path.to_str().expect("URI list path is UTF-8"),
    ])
    .expect("URI list arguments should parse");
    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("URI list input should load");

    assert_eq!(app.detected_inputs.len(), 1);
    assert_eq!(
        app.restore_session()
            .await
            .expect("URI list is not a session"),
        0
    );
}

#[tokio::test]
async fn test_no_conf_skips_explicit_config_file() {
    let temp_dir = TempDir::new().expect("temporary config directory");
    let config_path = temp_dir.path().join("aria2.conf");
    tokio::fs::write(&config_path, "split=8\n")
        .await
        .expect("write config file");

    let mut app = App::new();
    app.load_startup_config(true, config_path.to_str())
        .await
        .expect("--no-conf should not attempt to read the file");

    assert_eq!(app.get_opt_i64("split").await, Some(16));
}

#[tokio::test]
async fn test_config_file_error_reports_invalid_option() {
    let temp_dir = TempDir::new().expect("temporary config directory");
    let config_path = temp_dir.path().join("aria2.conf");
    tokio::fs::write(&config_path, "split=not-a-number\n")
        .await
        .expect("write config file");

    let mut app = App::new();
    let error = app
        .load_startup_config(false, config_path.to_str())
        .await
        .expect_err("invalid config value should fail startup");

    assert!(
        error.contains("split"),
        "error should name the option: {error}"
    );
    assert!(
        error.contains("invalid integer"),
        "error should explain the invalid value: {error}"
    );
    assert!(
        error.contains(":1:"),
        "error should include the line: {error}"
    );
    assert!(
        error.contains("split=not-a-number"),
        "error should include the source line: {error}"
    );
}

#[tokio::test]
async fn test_check_config_validates_without_starting_downloads() {
    let temp_dir = TempDir::new().expect("temporary config directory");
    let config_path = temp_dir.path().join("aria2.conf");
    tokio::fs::write(&config_path, "split=4\nfile-allocation=none\n")
        .await
        .expect("write config file");

    let mut app = App::new();
    app.check_config(false, config_path.to_str())
        .await
        .expect("valid config should pass checking");
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_standard_session_restores_metalink_graph() {
    use aria2_core::session::session_entry::SessionEntry;

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let metadata_path = temp_dir.path().join("metadata.torrent");
    let mut options = HashMap::new();
    options.insert(
        "dir".to_string(),
        temp_dir.path().to_string_lossy().into_owned(),
    );
    options.insert(
        "aria2-rust-payload-gid".to_string(),
        "0000000000000020".to_string(),
    );
    options.insert(
        "aria2-rust-metadata-uri".to_string(),
        "https://example.test/metadata.torrent".to_string(),
    );
    options.insert(
        "aria2-rust-metadata-path".to_string(),
        metadata_path.to_string_lossy().into_owned(),
    );
    options.insert("aria2-rust-metadata-memory".to_string(), "true".to_string());
    options.insert(
        "aria2-rust-output-name".to_string(),
        "payload.bin".to_string(),
    );
    let entry = SessionEntry::new(
        0x10,
        vec!["https://example.test/metadata.torrent".to_string()],
    )
    .with_options(options);
    tokio::fs::write(&session_file, entry.serialize())
        .await
        .expect("write session");

    let app = App::new();
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }

    assert_eq!(app.restore_session().await.expect("restore session"), 2);
    let groups = app.request_man.list_groups();
    let metadata = groups
        .iter()
        .find(|group| {
            group.recover().gid() == aria2_core::request::request_group::GroupId::new(0x10)
        })
        .expect("metadata group should be restored");
    assert_eq!(
        metadata.recover().belongs_to_gid(),
        Some(aria2_core::request::request_group::GroupId::new(0x20))
    );
    let payload = groups
        .iter()
        .find(|group| {
            group.recover().gid() == aria2_core::request::request_group::GroupId::new(0x20)
        })
        .expect("payload group should be restored");
    assert_eq!(
        payload.recover().output_name().as_deref(),
        Some("payload.bin")
    );
    assert!(!payload.recover().is_dependency_resolved());
    let payload_options = payload
        .recover()
        .effective_option_snapshot()
        .expect("restored payload should retain a request option snapshot");
    let expected_dir = temp_dir.path().to_string_lossy().into_owned();
    assert_eq!(
        payload_options
            .get("dir")
            .and_then(serde_json::Value::as_str),
        Some(expected_dir.as_str())
    );
    assert!(
        !payload_options.contains_key("aria2-rust-metadata-uri"),
        "session metadata must not be observable through task options"
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_session_save_then_restart_restores_metalink_graph() {
    use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
    use aria2_core::request::request_group::{DownloadOptions, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let options = DownloadOptions {
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        out: Some("payload.bin".to_string()),
        ..Default::default()
    };
    let graph = MetalinkRequestGraph::new_memory_with_fallback(
        "https://example.test/payload.torrent",
        "payload.bin",
        &options,
        GroupId::new(0x30),
        GroupId::new(0x40),
        vec!["https://mirror.example.test/payload.bin".to_string()],
    )
    .expect("graph should be constructible");

    let app = App::new();
    app.request_man
        .add_metalink_graph(graph)
        .expect("graph should be queued");
    let payload = app
        .request_man
        .find_group(GroupId::new(0x40))
        .expect("payload group should be indexed");
    payload.recover().set_bt_bitfield(Some(vec![0xa5, 0x03]));
    payload.recover().set_bt_metadata(
        11,
        16_384,
        "1111111111111111111111111111111111111111".to_string(),
    );
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }

    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1),
        "only the dependency-gated payload should be persisted"
    );

    let session_text = tokio::fs::read_to_string(&session_file)
        .await
        .expect("saved session should be readable");
    assert!(session_text.contains("aria2-rust-payload-gid=0000000000000040"));
    assert!(session_text.contains("aria2-rust-metadata-uri=https://example.test/payload.torrent"));
    let saved_entry =
        aria2_core::session::active_session::ActiveSessionManager::new(session_file.clone())
            .load_session()
            .await
            .expect("saved graph session should load");
    assert_eq!(saved_entry.len(), 1);
    assert_eq!(saved_entry[0].bitfield, Some(vec![0xa5, 0x03]));
    assert_eq!(saved_entry[0].num_pieces, Some(11));
    assert_eq!(saved_entry[0].piece_length, Some(16_384));
    assert_eq!(
        saved_entry[0].info_hash_hex.as_deref(),
        Some("1111111111111111111111111111111111111111")
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }

    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        2,
        "restart must rebuild both metadata and payload groups"
    );
    let groups = restarted.request_man.list_groups();
    let metadata = groups
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x30))
        .expect("metadata group should be restored");
    assert_eq!(
        metadata.recover().belongs_to_gid(),
        Some(GroupId::new(0x40))
    );

    let payload = groups
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x40))
        .expect("payload group should be restored");
    assert_eq!(
        payload.recover().output_name().as_deref(),
        Some("payload.bin")
    );
    assert_eq!(payload.recover().get_bt_bitfield(), Some(vec![0xa5, 0x03]));
    assert_eq!(payload.recover().get_bt_num_pieces(), 11);
    assert_eq!(payload.recover().get_bt_piece_length(), 16_384);
    assert_eq!(
        payload.recover().get_bt_info_hash_hex().as_deref(),
        Some("1111111111111111111111111111111111111111")
    );
    assert!(!payload.recover().is_dependency_resolved());
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_process_restart_executes_restored_metalink_graph() {
    use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
    use aria2_core::request::request_group::{DownloadStatus, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let (metadata_uri, request_count, metadata_server) =
        spawn_torrent_metadata_server(
            b"d8:announce27:http://127.0.0.1:1/announce4:infod6:lengthi0e4:name9:empty.bin12:piece lengthi16384e6:pieces0:ee".to_vec(),
        )
        .await;
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        out: Some("empty.bin".to_string()),
        enable_dht: false,
        enable_public_trackers: false,
        seed_time: Some(0.0),
        ..Default::default()
    };
    let metadata_gid = GroupId::new(0x70);
    let payload_gid = GroupId::new(0x80);
    let graph = MetalinkRequestGraph::new_memory(
        &metadata_uri,
        "empty.bin",
        &options,
        metadata_gid,
        payload_gid,
    )
    .expect("graph should be constructible");

    let app = App::new();
    app.request_man
        .add_metalink_graph(graph)
        .expect("graph should be queued");
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }
    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1),
        "only the dependency-gated payload should be persisted"
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }
    restarted.initialize_engine().await;
    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        2,
        "process restart must rebuild both metadata and payload groups"
    );

    restarted
        .run_engine(
            super::startup::StartupPlan::resolve(super::startup::StartupInputs {
                has_initial_downloads: true,
                has_input_file: false,
                restored_tasks: 0,
                tui: false,
                configured_rpc: false,
                explicit_rpc: None,
            })
            .unwrap(),
            false,
        )
        .await
        .expect("restored Metalink graph should execute to completion");
    metadata_server
        .await
        .expect("metadata server task should not panic");

    assert_eq!(request_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&metadata_gid.to_hex_string())
            .expect("completed metadata group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&payload_gid.to_hex_string())
            .expect("completed payload group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
    assert_eq!(
        tokio::fs::read(temp_dir.path().join("empty.bin"))
            .await
            .expect("completed zero-length payload should exist"),
        Vec::<u8>::new()
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_process_restart_executes_nonzero_metalink_graph_from_checkpoint() {
    use aria2_core::checksum::message_digest::{HashType, MessageDigest};
    use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
    use aria2_core::filesystem::control_file::ControlFile;
    use aria2_core::request::request_group::{DownloadStatus, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let output_path = temp_dir.path().join("payload.bin");
    let payload_bytes = b"abcdefgh".to_vec();

    let mut piece_hashes = Vec::with_capacity(40);
    for piece in payload_bytes.chunks(4) {
        piece_hashes.extend(MessageDigest::hash_data(HashType::Sha1, piece));
    }
    let mut info = b"d6:lengthi8e4:name11:payload.bin12:piece lengthi4e6:pieces40:".to_vec();
    info.extend_from_slice(&piece_hashes);
    info.push(b'e');
    let mut torrent = b"d8:announce27:http://127.0.0.1:1/announce4:info".to_vec();
    torrent.extend_from_slice(&info);
    torrent.push(b'e');
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("nonzero torrent fixture should parse");
    let info_hash = metadata.info_hash.bytes;
    let info_hash_hex = metadata.info_hash.as_hex();

    tokio::fs::write(&output_path, &payload_bytes)
        .await
        .expect("pre-existing payload should be writable");
    let control_path = ControlFile::control_path_for(&output_path);
    let mut control = ControlFile::open_or_create(&control_path, 8, 2)
        .await
        .expect("checkpoint should be constructible");
    control.mark_torrent_checkpoint();
    control.set_torrent_info_hash(info_hash);
    control.set_torrent_piece_length(4);
    control.set_bitfield(vec![0xc0]);
    control.update_completed_length(8);
    control.save().await.expect("checkpoint should be durable");

    let (metadata_uri, request_count, metadata_server) =
        spawn_torrent_metadata_server(torrent).await;
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        out: Some("payload.bin".to_string()),
        enable_dht: false,
        enable_public_trackers: false,
        seed_time: Some(0.0),
        ..Default::default()
    };
    let metadata_gid = GroupId::new(0x90);
    let payload_gid = GroupId::new(0xa0);
    let graph = MetalinkRequestGraph::new_memory(
        &metadata_uri,
        "payload.bin",
        &options,
        metadata_gid,
        payload_gid,
    )
    .expect("graph should be constructible");

    let app = App::new();
    app.request_man
        .add_metalink_graph(graph)
        .expect("graph should be queued");
    let payload = app
        .request_man
        .find_group(payload_gid)
        .expect("payload group should be indexed");
    payload.recover().set_bt_bitfield(Some(vec![0xc0]));
    payload.recover().set_bt_metadata(2, 4, info_hash_hex);
    payload.recover().set_total_length(8);
    payload.recover().set_completed_length(8);
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }
    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1),
        "only the dependency-gated payload should be persisted"
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }
    restarted.initialize_engine().await;
    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        2,
        "process restart must rebuild both metadata and payload groups"
    );

    restarted
        .run_engine(
            super::startup::StartupPlan::resolve(super::startup::StartupInputs {
                has_initial_downloads: true,
                has_input_file: false,
                restored_tasks: 0,
                tui: false,
                configured_rpc: false,
                explicit_rpc: None,
            })
            .unwrap(),
            false,
        )
        .await
        .expect("restored nonzero Metalink graph should execute to completion");
    metadata_server
        .await
        .expect("metadata server task should not panic");

    assert_eq!(request_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        tokio::fs::read(&output_path)
            .await
            .expect("payload should remain readable"),
        payload_bytes
    );
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&metadata_gid.to_hex_string())
            .expect("completed metadata group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&payload_gid.to_hex_string())
            .expect("completed payload group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_process_restart_executes_paused_metalink_graph_after_unpause() {
    use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
    use aria2_core::request::request_group::{DownloadStatus, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let (metadata_uri, request_count, metadata_server) = spawn_torrent_metadata_server(
        b"d8:announce27:http://127.0.0.1:1/announce4:infod6:lengthi0e4:name9:empty.bin12:piece lengthi16384e6:pieces0:ee".to_vec(),
    )
    .await;
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        out: Some("empty.bin".to_string()),
        enable_dht: false,
        enable_public_trackers: false,
        seed_time: Some(0.0),
        ..Default::default()
    };
    let metadata_gid = GroupId::new(0xb0);
    let payload_gid = GroupId::new(0xc0);
    let graph = MetalinkRequestGraph::new_memory(
        &metadata_uri,
        "empty.bin",
        &options,
        metadata_gid,
        payload_gid,
    )
    .expect("graph should be constructible");

    let app = App::new();
    app.request_man
        .add_metalink_graph(graph)
        .expect("graph should be queued");
    app.request_man
        .find_group(payload_gid)
        .expect("payload group should be indexed")
        .recover_mut()
        .pause()
        .expect("payload should be pausable before session save");
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }
    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1),
        "paused graph should persist its dependency-gated payload"
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }
    restarted.initialize_engine().await;
    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        2,
        "process restart must rebuild both paused graph groups"
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(metadata_gid)
            .expect("metadata group should be restored")
            .recover()
            .status(),
        DownloadStatus::Paused
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(payload_gid)
            .expect("payload group should be restored")
            .recover()
            .status(),
        DownloadStatus::Paused
    );

    restarted
        .request_man
        .unpause_group(metadata_gid)
        .expect("restored paused graph should be unpausable");
    assert_eq!(
        restarted
            .request_man
            .find_group(metadata_gid)
            .expect("metadata group should remain indexed")
            .recover()
            .status(),
        DownloadStatus::Waiting
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(payload_gid)
            .expect("payload group should remain indexed")
            .recover()
            .status(),
        DownloadStatus::Waiting,
        "unpausing the restored metadata GID must resume its payload graph"
    );

    restarted
        .run_engine(
            super::startup::StartupPlan::resolve(super::startup::StartupInputs {
                has_initial_downloads: true,
                has_input_file: false,
                restored_tasks: 0,
                tui: false,
                configured_rpc: false,
                explicit_rpc: None,
            })
            .unwrap(),
            false,
        )
        .await
        .expect("unpaused restored Metalink graph should execute to completion");
    metadata_server
        .await
        .expect("metadata server task should not panic");

    assert_eq!(request_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&metadata_gid.to_hex_string())
            .expect("completed metadata group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
    assert_eq!(
        restarted
            .request_man
            .find_stopped_result(&payload_gid.to_hex_string())
            .expect("completed payload group should be in stopped results")
            .status,
        DownloadStatus::Complete
    );
    assert_eq!(
        tokio::fs::read(temp_dir.path().join("empty.bin"))
            .await
            .expect("completed zero-length payload should exist"),
        Vec::<u8>::new()
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_paused_session_graph_unpauses_both_groups() {
    use aria2_core::engine::metalink_request_graph::MetalinkRequestGraph;
    use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let options = DownloadOptions {
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        out: Some("paused-payload.bin".to_string()),
        ..Default::default()
    };
    let metadata_gid = GroupId::new(0x50);
    let payload_gid = GroupId::new(0x60);
    let graph = MetalinkRequestGraph::new_memory_with_fallback(
        "https://example.test/paused-payload.torrent",
        "paused-payload.bin",
        &options,
        metadata_gid,
        payload_gid,
        vec!["https://mirror.example.test/paused-payload.bin".to_string()],
    )
    .expect("graph should be constructible");

    let app = App::new();
    app.request_man
        .add_metalink_graph(graph)
        .expect("graph should be queued");
    app.request_man
        .find_group(payload_gid)
        .expect("payload group should be indexed")
        .recover_mut()
        .pause()
        .expect("payload should be pausable");
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }
    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1)
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }
    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        2
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(metadata_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Paused
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(payload_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Paused
    );

    restarted
        .request_man
        .unpause_group(metadata_gid)
        .expect("session task should be unpausable");
    assert_eq!(
        restarted
            .request_man
            .find_group(metadata_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Waiting
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(payload_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Waiting,
        "unpausing the persisted metadata GID must resume its payload graph"
    );

    restarted
        .request_man
        .force_pause_group(metadata_gid)
        .expect("session task should be force-pausable");
    assert_eq!(
        restarted
            .request_man
            .find_group(metadata_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Paused
    );
    assert_eq!(
        restarted
            .request_man
            .find_group(payload_gid)
            .unwrap()
            .recover()
            .status(),
        DownloadStatus::Paused,
        "force-pausing the persisted metadata GID must pause its payload graph"
    );
}

#[tokio::test]
async fn test_force_saved_stopped_result_survives_app_session_restart() {
    use aria2_core::request::request_group::{DownloadStatus, GroupId};

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session.gz");
    let app = App::new();
    let gid = app
        .request_man
        .add_group(
            vec!["https://example.test/complete.bin".to_string()],
            DownloadOptions {
                out: Some("complete.bin".to_string()),
                split: Some(4),
                ..Default::default()
            },
        )
        .expect("group should be created");

    app.request_man.fill_from_reserver();
    let group = app
        .request_man
        .find_group(gid)
        .expect("group should be promoted");
    group.recover_mut().set_option_snapshot(HashMap::from([
        ("force-save".to_string(), serde_json::json!(true)),
        ("split".to_string(), serde_json::json!("4")),
        ("out".to_string(), serde_json::json!("complete.bin")),
    ]));
    group.recover().mark_complete();
    assert_eq!(
        app.request_man.remove_stopped_groups(None),
        vec![gid],
        "completed group should enter stopped storage"
    );

    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }

    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("save session should succeed"),
        Some(1),
        "force-saved stopped result should be counted"
    );
    let session_bytes = tokio::fs::read(&session_file)
        .await
        .expect("saved compressed session should be readable");
    assert_eq!(&session_bytes[..2], &[0x1f, 0x8b], "session should be gzip");
    let entries =
        aria2_core::session::active_session::ActiveSessionManager::new(session_file.clone())
            .load_session()
            .await
            .expect("saved compressed session should load");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].uris,
        vec!["https://example.test/complete.bin".to_string()]
    );
    assert_eq!(
        entries[0].options.get("force-save"),
        Some(&"true".to_string())
    );

    let restarted = App::new();
    {
        let mut config = restarted.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }

    assert_eq!(
        restarted
            .restore_session()
            .await
            .expect("restore session should succeed"),
        1,
        "force-saved stopped result should restore as one waiting task"
    );
    let restored = restarted
        .request_man
        .find_group(GroupId::new(gid.value()))
        .expect("restored group should be indexed");
    assert_eq!(restored.recover().status(), DownloadStatus::Waiting);
    assert_eq!(
        restored.recover().options().out.as_deref(),
        Some("complete.bin")
    );
}

/// Test 1: Load entries from session file
///
/// Verify that restore_session() correctly loads and restores entries from a mock session file
#[tokio::test]
async fn test_input_file_loads_entries() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("test_session.txt");

    // Create a test session file with 3 entries
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/file1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=524288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=524288

http://example.com/file2.iso
 GID=2
 split=4
 dir=/downloads
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

ftp://server.com/bigfile.bin
 GID=3
 TOTAL_LENGTH=1073741824
 COMPLETED_LENGTH=536870912
 STATUS=paused
 ERROR_CODE=
 BITFIELD=fff00f
 NUM_PIECES=24
 PIECE_LENGTH=262144
 INFO_HASH=abc123def456
 RESUME_OFFSET=536870912
"#;

    // Write session file
    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    // Create App instance and configure input-file
    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    // Call restore method
    let result = app.restore_session().await;

    // Verify result
    assert!(result.is_ok(), "Restore should succeed");
    let count = result.unwrap();

    // Should restore 2 entries (skip file2 with completed_length=0 and total_length>0)
    // But according to our logic: completed_length=0 && total_length=0 is skipped
    // file2: completed_length=0, total_length=10485760 -> not skipped
    // So should restore 3 entries (none have complete status)
    assert_eq!(count, 3, "Should restore 3 non-completed entries");

    // Verify RequestGroupMan has corresponding groups
    let man = &app.request_man;
    let group_count = man.count();
    assert_eq!(group_count, 3, "RequestGroupMan should have 3 groups");
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(1))
            .is_some()
    );
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(2))
            .is_some()
    );
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(3))
            .is_some()
    );
}

/// Test 2: Skip completed entries
///
/// Verify that entries with status "complete" are correctly skipped during restoration
#[tokio::test]
async fn test_skip_completed_entries() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("test_complete_session.txt");

    // Create session file with completed entries
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/complete1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=1048576
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1048576

http://example.com/active2.zip
 GID=2
 TOTAL_LENGTH=2048576
 COMPLETED_LENGTH=1024288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1024288

http://example.com/complete3.bin
 GID=3
 TOTAL_LENGTH=512
 COMPLETED_LENGTH=512
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=512

http://example.com/paused4.iso
 GID=4
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 STATUS=paused
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Restore should succeed");
    let count = result.unwrap();

    // Should only restore 2 entries (active and paused), skip 2 complete
    assert_eq!(count, 2, "Should only restore 2 non-completed entries");

    let man = &app.request_man;
    let group_count = man.count();
    assert_eq!(group_count, 2, "RequestGroupMan should have 2 groups");
}

/// Test 3: Save session on shutdown
///
/// Verify that save_session_on_shutdown() correctly saves when save-session is configured
#[tokio::test]
async fn test_save_session_on_shutdown() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let save_file = temp_dir.path().join("shutdown_save.txt");

    let app = App::new();

    // Configure save-session option
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "save-session",
            OptionValue::Str(save_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set save-session");
        conf.set_global_option("save-session-interval", OptionValue::Str("60".to_string()))
            .await
            .expect("Failed to set save-session-interval");
    }

    // Add some download tasks to RequestGroupMan
    let opts = DownloadOptions {
        dir: Some("/downloads".to_string()),
        ..Default::default()
    };

    {
        let man = &app.request_man;
        man.add_group(
            vec!["http://example.com/file1.zip".to_string()],
            opts.clone(),
        )
        .expect("Failed to add group 1");

        man.add_group(vec!["http://mirror.com/file2.iso".to_string()], opts)
            .expect("Failed to add group 2");
    }

    // Call shutdown save
    let result = app.save_session_on_shutdown().await;

    // Verify result
    assert!(result.is_ok(), "Save should succeed");
    let saved_count = result.expect("Should have a return value");
    assert!(
        saved_count.is_some(),
        "Should return Some when save-session is configured"
    );
    assert_eq!(saved_count.unwrap(), 2, "Should save 2 active tasks");

    // Verify file was created and contains correct URIs
    assert!(save_file.exists(), "Session file should exist after save");

    let content = tokio::fs::read_to_string(&save_file)
        .await
        .expect("Failed to read saved file");
    assert!(
        content.contains("http://example.com/file1.zip"),
        "File should contain the first URI"
    );
    assert!(
        content.contains("http://mirror.com/file2.iso"),
        "File should contain the second URI"
    );
}

#[tokio::test]
async fn test_save_session_on_shutdown_clears_stale_file_without_groups() {
    let temp_dir = TempDir::new().expect("temporary session directory");
    let save_file = temp_dir.path().join("stale_shutdown_save.txt");
    tokio::fs::write(&save_file, "http://stale.example/old.bin\n")
        .await
        .expect("write stale session");

    let app = App::new();
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "save-session",
                OptionValue::Str(save_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session output");
    }

    assert_eq!(
        app.save_session_on_shutdown()
            .await
            .expect("clear session should succeed"),
        Some(0)
    );
    assert_eq!(
        tokio::fs::read_to_string(&save_file)
            .await
            .expect("read cleared session"),
        ""
    );
}

/// Test 4: No save when save-session is not configured
///
/// Verify that save_session_on_shutdown() returns Ok(None) when save-session is not configured
#[tokio::test]
async fn test_no_save_when_not_configured() {
    let app = App::new();

    // Do not configure save-session

    let result = app.save_session_on_shutdown().await;

    assert!(result.is_ok(), "Should return Ok when not configured");
    assert!(
        result.unwrap().is_none(),
        "Should return None when save-session is not configured"
    );
}

/// Test 5: map_entry_to_download_options correctly maps options
#[test]
fn test_map_entry_to_download_options() {
    let mut options = HashMap::new();
    options.insert("split".to_string(), "8".to_string());
    options.insert("dir".to_string(), "/tmp/downloads".to_string());
    options.insert("out".to_string(), "output.bin".to_string());
    options.insert("max-download-limit".to_string(), "102400".to_string());
    options.insert("bt-force-encrypt".to_string(), "true".to_string());
    options.insert("enable-dht".to_string(), "false".to_string());

    let opts = App::map_entry_to_download_options(&options);

    assert_eq!(opts.split, Some(8), "split should map correctly");
    assert_eq!(
        opts.dir,
        Some("/tmp/downloads".to_string()),
        "dir should map correctly"
    );
    assert_eq!(
        opts.out,
        Some("output.bin".to_string()),
        "out should map correctly"
    );
    assert_eq!(
        opts.max_download_limit,
        Some(102400),
        "max-download-limit should map correctly"
    );
    assert!(
        opts.bt_force_encrypt,
        "bt-force-encrypt=true should map correctly"
    );
    assert!(!opts.enable_dht, "enable-dht=false should map correctly");
}

/// Test 6: Graceful handling of non-existent session file
#[tokio::test]
async fn test_restore_nonexistent_session_file() {
    let app = App::new();

    // Configure to point to non-existent file
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str("/nonexistent/path/session.txt".to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;

    // Should return Ok(0) when file doesn't exist, not error
    assert!(result.is_ok(), "Should return Ok when file does not exist");
    assert_eq!(
        result.unwrap(),
        0,
        "Should return 0 restored entries when file does not exist"
    );
}

/// Test 7: No restore when input-file is not configured
#[tokio::test]
async fn test_restore_without_input_file() {
    let app = App::new();

    // Do not configure input-file

    let result = app.restore_session().await;

    assert!(result.is_ok(), "Should return Ok when not configured");
    assert_eq!(
        result.unwrap(),
        0,
        "Should return 0 when input-file is not configured"
    );
}

/// Test 8: BT bitfield preserved on restore
#[tokio::test]
async fn test_bt_bitfield_preserved_on_restore() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("bt_session.txt");

    // Create session entry with BT bitfield
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"magnet:?xt=urn:btih:abc123def456
 GID=1
 TOTAL_LENGTH=104857600
 COMPLETED_LENGTH=52428800
 STATUS=active
 ERROR_CODE=
 BITFIELD=ffaabb
 NUM_PIECES=20
 PIECE_LENGTH=5242880
 INFO_HASH=abc123def456
 RESUME_OFFSET=52428800
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Restore should succeed");
    assert_eq!(result.unwrap(), 1, "Should restore 1 BT task");

    // Verify bitfield is preserved in RequestGroup
    let man = &app.request_man;
    let groups = man.list_groups();
    assert_eq!(groups.len(), 1, "Should have 1 group");

    let group = groups[0].read().unwrap();
    let bitfield = group.bt_bitfield.read().unwrap();
    assert!(bitfield.is_some(), "BT bitfield should be preserved");
    assert_eq!(
        bitfield.as_ref().unwrap(),
        &vec![0xFF, 0xAA, 0xBB],
        "bitfield value should be correct"
    );
}

/// Test 9: Graceful handling of empty session file
#[tokio::test]
async fn test_restore_empty_session_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("empty_session.txt");

    // Create empty session file
    tokio::fs::write(&session_file, "")
        .await
        .expect("Failed to write empty file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Empty file should return Ok");
    assert_eq!(
        result.unwrap(),
        0,
        "Empty file should return 0 restored entries"
    );
}

/// Test 10: Skip entries with zero progress
#[tokio::test]
async fn test_skip_entries_with_zero_progress() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("zero_progress_session.txt");

    // Create session file where all entries have no progress
    // Per C++ aria2 behavior, 0/0 entries are still restored (they may
    // be newly added downloads that haven't started yet). Only "removed"
    // entries are skipped.
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/new1.zip
 GID=1
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

http://example.com/new2.iso
 GID=2
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Should return Ok");
    // C++ aria2 restores ALL non-finished entries, including 0/0 progress
    // entries (they may be newly added downloads). Only "removed" entries
    // are skipped.
    assert_eq!(
        result.unwrap(),
        2,
        "C++ aria2 restores all non-finished entries including 0/0 progress"
    );

    let man = &app.request_man;
    let group_count = man.count();
    assert_eq!(group_count, 2, "Should restore both 0/0 progress groups");
}
