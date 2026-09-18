use super::discovery::prepare_tracker_tiers;
use super::policy::{download_speed_is_below_peer_request_limit, effective_peer_speed_threshold};
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_download_command_tests::build_test_torrent;
use crate::engine::lpd_manager::LpdManager;
use crate::request::request_group::{DownloadOptions, GroupId};
use std::sync::Arc;

#[tokio::test]
async fn lpd_registers_public_torrent_before_empty_peer_results() {
    let torrent = build_test_torrent();
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent).unwrap();
    let options = DownloadOptions {
        bt_enable_lpd: true,
        enable_dht: false,
        enable_public_trackers: false,
        bt_exclude_tracker: Some(vec!["*".to_string()]),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(GroupId::new(7001), &torrent, &options, None)
        .expect("test torrent should construct");
    let manager = Arc::new(LpdManager::new());
    command.set_lpd_manager(Arc::clone(&manager));

    let peers = command
        .discover_peers(&meta, meta.total_size(), &meta.network_info_hash())
        .await
        .expect("discovery without network trackers should succeed");

    assert!(peers.is_empty());
    assert!(
        manager
            .active_hashes
            .read()
            .await
            .contains(meta.info_hash.as_hex().as_str()),
        "LPD must register a public torrent even when no peer is discovered"
    );
}

#[test]
fn tracker_exclusions_and_user_trackers_follow_announce_policy() {
    let tiers = prepare_tracker_tiers(
        vec![vec![
            "http://torrent-one.test/announce".to_string(),
            "http://torrent-two.test/announce".to_string(),
        ]],
        "",
        Some(vec!["http://custom.test/announce".to_string()]),
        &["http://torrent-one.test/announce".to_string()],
    );

    assert_eq!(
        tiers,
        vec![
            vec!["http://torrent-two.test/announce".to_string()],
            vec!["http://custom.test/announce".to_string()],
        ]
    );
}

#[test]
fn wildcard_tracker_exclusion_removes_torrent_trackers_but_keeps_override() {
    let tiers = prepare_tracker_tiers(
        vec![vec!["http://torrent.test/announce".to_string()]],
        "",
        Some(vec!["http://custom.test/announce".to_string()]),
        &["*".to_string()],
    );

    assert_eq!(tiers, vec![vec!["http://custom.test/announce".to_string()]]);
}

#[test]
fn peer_speed_threshold_is_clamped_by_download_limit() {
    assert_eq!(
        effective_peer_speed_threshold(50 * 1024, Some(20 * 1024)),
        20 * 1024
    );
    assert_eq!(
        effective_peer_speed_threshold(50 * 1024, Some(0)),
        50 * 1024
    );
    assert_eq!(effective_peer_speed_threshold(50 * 1024, None), 50 * 1024);
}

#[test]
fn low_peer_speed_requests_more_peers_but_zero_disables_policy() {
    assert!(download_speed_is_below_peer_request_limit(
        10 * 1024,
        50 * 1024,
        None,
    ));
    assert!(!download_speed_is_below_peer_request_limit(
        50 * 1024,
        50 * 1024,
        None,
    ));
    assert!(!download_speed_is_below_peer_request_limit(0, 0, None));
}
