use super::*;

#[test]
fn normalize_bep9_info_metadata_builds_a_complete_torrent() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let torrent = build_test_torrent();
    let (root, consumed) = BencodeValue::decode(&torrent).expect("decode test torrent");
    assert_eq!(consumed, torrent.len());
    let info = root.dict_get(b"info").expect("test torrent info dict");
    let info_bytes = info.encode();
    let expected_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .unwrap()
        .info_hash;
    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
        "magnet:?xt=urn:btih:{}&tr=http%3A%2F%2Ftracker.test%2Fannounce",
        expected_hash.as_hex()
    ))
    .expect("test magnet should parse");

    let normalized = MagnetDownloadCommand::normalize_magnet_metadata(&magnet, &info_bytes)
        .expect("BEP 9 info metadata should be wrapped");
    let parsed = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&normalized)
        .expect("normalized metadata should parse as a torrent");

    assert_eq!(parsed.info_hash.bytes, magnet.info_hash);
    assert_eq!(parsed.announce, "http://tracker.test/announce");
    assert_eq!(parsed.info_hash.bytes, expected_hash.bytes);
}

#[test]
fn normalize_magnet_metadata_keeps_complete_torrent_bytes() {
    let torrent = build_test_torrent();
    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(
        "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678",
    )
    .expect("test magnet should parse");

    let normalized = MagnetDownloadCommand::normalize_magnet_metadata(&magnet, &torrent)
        .expect("complete torrent metadata should remain valid");
    assert_eq!(normalized, torrent);
}

#[test]
fn magnet_web_seeds_are_added_without_changing_info_hash() {
    let torrent = build_test_torrent();
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent should parse");
    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(&format!(
            "magnet:?xt=urn:btih:{}&ws=https%3A%2F%2Fseed.example%2Ffiles%2F&ws=https%3A%2F%2Fseed.example%2Fmirror%2F",
            meta.info_hash.as_hex()
        ))
        .expect("web-seed magnet should parse");

    let merged = MagnetDownloadCommand::merge_magnet_web_seeds(&magnet, &torrent)
        .expect("web seeds should be merged into torrent metadata");
    let merged_meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&merged)
        .expect("merged torrent should parse");

    assert_eq!(merged_meta.info_hash, meta.info_hash);
    assert_eq!(
        merged_meta.web_seeds,
        vec![
            "https://seed.example/files/".to_string(),
            "https://seed.example/mirror/".to_string(),
        ]
    );
}

#[test]
fn saved_metadata_is_loaded_only_when_info_hash_matches() {
    let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
    let command = make_test_command_in_dir(temp_dir.path());
    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent parses")
        .info_hash
        .bytes;
    let path = command.saved_metadata_path(&info_hash);
    std::fs::write(&path, &torrent).expect("write saved metadata");

    assert_eq!(command.load_saved_metadata(&info_hash), Some(torrent));
    let mismatched_path = command.saved_metadata_path(&[0u8; 20]);
    std::fs::write(&mismatched_path, build_test_torrent())
        .expect("write mismatched saved metadata");
    assert!(command.load_saved_metadata(&[0u8; 20]).is_none());
}

#[tokio::test]
async fn magnet_metadata_options_drive_saved_load_and_metadata_only_execution() {
    let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
    let torrent = build_test_torrent();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
        .expect("test torrent parses")
        .info_hash
        .bytes;
    let magnet = format!(
        "magnet:?xt=urn:btih:{}&dn=test_file",
        hex::encode(info_hash)
    );
    let options = DownloadOptions {
        bt_load_saved_metadata: true,
        bt_save_metadata: true,
        bt_metadata_only: true,
        enable_dht: false,
        ..DownloadOptions::default()
    };
    let mut command =
        MagnetDownloadCommand::new(GroupId::new(3), &magnet, &options, temp_dir.path().to_str())
            .expect("magnet command should be constructible");
    let saved_path = command.saved_metadata_path(&info_hash);
    std::fs::write(&saved_path, &torrent).expect("write saved torrent metadata");

    let listener = Arc::new(MetadataEventListener::new());
    DownloadEventHooks::shared().add_listener(listener.clone());

    command
        .execute()
        .await
        .expect("saved metadata should avoid network discovery");

    assert_eq!(command.status(), CommandStatus::Completed);
    assert_eq!(
        command.group().status(),
        crate::request::request_group::DownloadStatus::Complete
    );
    assert_eq!(
        std::fs::read(saved_path).expect("read saved torrent"),
        torrent
    );
    assert_eq!(
        listener.events.lock().unwrap().as_slice(),
        [MetadataResolvedEvent::new(GroupId::new(3), Vec::new())]
    );
    listener
        .alive
        .store(false, std::sync::atomic::Ordering::Release);
}

#[test]
fn saving_metadata_never_overwrites_an_existing_file() {
    let temp_dir = tempfile::tempdir().expect("temporary metadata directory");
    let path = temp_dir.path().join("metadata.torrent");
    std::fs::write(&path, b"original").expect("write existing metadata");

    assert!(
        !MagnetDownloadCommand::save_metadata_file(&path, b"replacement")
            .expect("create_new should not fail for an existing file")
    );
    assert_eq!(
        std::fs::read(&path).expect("read existing metadata"),
        b"original"
    );

    let new_path = temp_dir.path().join("new.torrent");
    assert!(
        MagnetDownloadCommand::save_metadata_file(&new_path, b"metadata")
            .expect("write new metadata")
    );
    assert_eq!(
        std::fs::read(new_path).expect("read new metadata"),
        b"metadata"
    );
}

#[test]
fn metadata_only_command_reports_completion_without_payload_bytes() {
    let mut command = make_test_command();
    assert_eq!(command.status(), CommandStatus::Pending);
    command.metadata_complete = true;
    assert_eq!(command.status(), CommandStatus::Completed);
}

/// When DHT was never started (e.g. enable_dht = false), the enforcement
/// method must still parse the metadata and succeed without error. There
/// is nothing to shut down, so the DHT engine set stays empty.
#[tokio::test]
async fn test_magnet_enforce_bep0027_no_dht_engine() {
    let mut cmd = make_test_command();
    assert!(cmd.dht_engines.is_empty(), "precondition: no DHT engine");

    let torrent_bytes = build_private_test_torrent();

    cmd.enforce_bep0027_after_metadata(&torrent_bytes)
        .await
        .expect("should succeed even when DHT engine is absent");

    assert!(cmd.dht_engines.is_empty());
}

/// Corrupt metadata bytes must produce a fatal config error rather than
/// silently treating the torrent as public (which would leak DHT usage
/// for what might actually be a private torrent).

#[tokio::test]
async fn test_magnet_enforce_bep0027_invalid_metadata_errors() {
    let mut cmd = make_test_command();

    let bad_bytes: &[u8] = b"this is not valid bencode";

    let result = cmd.enforce_bep0027_after_metadata(bad_bytes).await;
    assert!(
        result.is_err(),
        "Invalid metadata bytes must return an error, not silently default to public"
    );

    // DHT engine should be untouched when parsing fails (fail-closed on
    // the parse error, but we do not preemptively shut down DHT since the
    // caller may want to retry metadata fetch from a different peer).
    assert!(
        cmd.dht_engines.is_empty(),
        "DHT engine set should be unchanged on parse error"
    );
}
