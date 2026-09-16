//! Basic status and BitTorrent DTO tests.

use crate::types::*;

#[test]
fn test_status_info_default() {
    let info = StatusInfo::default();
    assert!(info.gid.is_empty());
    assert_eq!(info.progress_percent(), 0.0);
}

#[test]
fn test_status_info_builder() {
    let info = StatusInfo::new("abc123")
        .with_total_length(1000)
        .with_completed_length(500)
        .with_download_speed(1024)
        .with_status(DownloadStatus::Active);
    assert_eq!(info.gid, "abc123");
    assert!((info.progress_percent() - 50.0).abs() < 0.01);
}

#[test]
fn test_download_status_variants() {
    assert!(DownloadStatus::Active.is_active());
    assert!(DownloadStatus::Complete.is_stopped());
    assert!(DownloadStatus::Removed.is_stopped());
    assert_eq!(DownloadStatus::Error("test".to_string()).as_str(), "error");
}

#[test]
fn test_bittorrent_info_serialization() {
    let bt = BittorrentInfo {
        announce_list: vec![
            vec!["udp://tracker1:80".to_string()],
            vec!["http://tracker2:80".to_string()],
        ],
        comment: Some("Test torrent".to_string()),
        creation_date: Some(1700000000),
        mode: Some("single".to_string()),
        info: Some(BittorrentMetaInfo {
            name: "test-file.iso".to_string(),
        }),
    };

    let json = serde_json::to_value(&bt).unwrap();
    assert_eq!(json["announceList"][0][0], "udp://tracker1:80");
    assert_eq!(json["comment"], "Test torrent");
    assert_eq!(json["creationDate"], 1700000000);
    assert_eq!(json["mode"], "single");
    assert_eq!(json["info"]["name"], "test-file.iso");

    let info = StatusInfo::new("gid-bt-001").with_bittorrent(bt);
    let serialized = serde_json::to_value(&info).unwrap();
    assert!(
        serialized.get("bittorrent").is_some(),
        "bittorrent field should appear in serialized StatusInfo"
    );
    let bt_json = serialized.get("bittorrent").unwrap();
    assert_eq!(bt_json["announceList"][0][0], "udp://tracker1:80");
    assert_eq!(bt_json["info"]["name"], "test-file.iso");

    let default_info = StatusInfo::default();
    let default_serialized = serde_json::to_value(&default_info).unwrap();
    assert!(
        default_serialized.get("bittorrent").is_none(),
        "Default StatusInfo should not have bittorrent field"
    );
}

#[test]
fn test_status_info_following_field() {
    let info = StatusInfo::new("gid-test".to_string()).with_following("gid-following-001");
    let serialized = serde_json::to_value(&info).unwrap();
    assert_eq!(
        serialized.get("following").unwrap().as_str().unwrap(),
        "gid-following-001",
        "following should be a single GID string"
    );

    let default_info = StatusInfo::default();
    let default_serialized = serde_json::to_value(&default_info).unwrap();
    assert!(
        default_serialized.get("following").is_none(),
        "Default StatusInfo should not have following field"
    );
}
