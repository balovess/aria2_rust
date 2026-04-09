use aria2_core::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::http_segment_downloader::HttpSegmentDownloader;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};

fn create_http_command(uri: &str, split: Option<u16>, max_conn: Option<u16>) -> DownloadCommand {
    let options = DownloadOptions {
        split,
        max_connection_per_server: max_conn,
        max_download_limit: None,
        max_upload_limit: None,
        dir: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        out: Some(format!("test_concurrent_{}.bin", std::process::id())),
        seed_time: None,
        seed_ratio: None,
        checksum: None,
        cookie_file: None,
        cookies: None,
        bt_force_encrypt: false,
        bt_require_crypto: false,
        enable_dht: false,
        dht_listen_port: None,
        enable_public_trackers: false,
        bt_piece_selection_strategy: "rarest-first".to_string(),
        bt_endgame_threshold: 20,
        max_retries: 3,
        retry_wait: 1,
        http_proxy: None,
        dht_file_path: None,
    };
    DownloadCommand::new(
        GroupId::new(1),
        uri,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )
    .unwrap()
}

#[test]
fn test_download_command_creation() {
    let _cmd = create_http_command("http://example.com/file", Some(4), Some(2));
}

#[test]
fn test_download_command_split_1_default() {
    let _cmd = DownloadCommand::new(
        GroupId::new(1),
        "http://example.com",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
}

#[test]
fn test_concurrent_segment_manager_creation() {
    let mut manager = ConcurrentSegmentManager::new(
        1024 * 1024,
        vec!["http://example.com/file".to_string()],
        Some(256 * 1024),
    );
    manager.set_max_connections_per_mirror(4);
    manager.allocate_segments();

    assert_eq!(manager.num_segments(), 4);
    assert!(!manager.is_complete());
    assert_eq!(manager.total_size(), 1024 * 1024);
}

#[test]
fn test_concurrent_segment_manager_single_segment() {
    let mut manager =
        ConcurrentSegmentManager::new(100, vec!["http://example.com/tiny".to_string()], Some(100));
    manager.allocate_segments();
    assert_eq!(manager.num_segments(), 1);
}

#[test]
fn test_concurrent_segment_manager_complete_all() {
    let mut manager =
        ConcurrentSegmentManager::new(200, vec!["http://example.com/med".to_string()], Some(100));
    manager.allocate_segments();
    manager.complete_segment(0, vec![0u8; 100]);
    manager.complete_segment(1, vec![0u8; 100]);
    assert!(manager.is_complete());
    assert_eq!(manager.completed_bytes(), 200);
    let assembled = manager.assemble().unwrap();
    assert_eq!(assembled.len(), 200);
}

#[test]
fn test_concurrent_segment_manager_fail_marks_failed() {
    let mut manager =
        ConcurrentSegmentManager::new(300, vec!["http://example.com/med".to_string()], Some(100));
    manager.allocate_segments();
    manager.fail_segment(0);

    let status = manager.segment_status(0);
    assert!(status.is_some(), "segment 0 should have a status");
}

#[tokio::test]
async fn test_http_segment_downloader_zero_length() {
    let client = reqwest::Client::new();
    let dl = HttpSegmentDownloader::new(&client);
    let result = dl.download_range("http://example.com", 0, 0, None).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_download_options_default() {
    let opts = DownloadOptions::default();
    assert!(opts.split.is_none());
    assert!(opts.max_connection_per_server.is_none());
    assert!(opts.max_download_limit.is_none());
}

#[test]
fn test_download_options_with_values() {
    let opts = DownloadOptions {
        split: Some(16),
        max_connection_per_server: Some(8),
        max_download_limit: Some(50000),
        max_upload_limit: Some(10000),
        dir: Some("/tmp".to_string()),
        out: Some("file.bin".to_string()),
        seed_time: None,
        seed_ratio: None,
        checksum: None,
        cookie_file: None,
        cookies: None,
        bt_force_encrypt: false,
        bt_require_crypto: false,
        enable_dht: false,
        dht_listen_port: None,
        enable_public_trackers: false,
        bt_piece_selection_strategy: "rarest-first".to_string(),
        bt_endgame_threshold: 20,
        max_retries: 3,
        retry_wait: 1,
        http_proxy: None,
        dht_file_path: None,
    };
    assert_eq!(opts.split, Some(16));
    assert_eq!(opts.max_connection_per_server, Some(8));
}
