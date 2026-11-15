use super::*;

#[test]
fn fill_from_group_derives_ddl_filename_when_no_output_is_configured() {
    let group = crate::request::request_group::RequestGroup::new(
        GroupId::new(1),
        vec!["https://example.com/releases/file.zip?download=1".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    );
    group.set_total_length(4096);

    let mut result = DownloadResult::finished();
    result.fill_from_group(&group);

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "file.zip");
    assert_eq!(result.files[0].length, 4096);
}

#[test]
fn fill_from_group_uses_the_safe_decoded_url_segment() {
    let group = crate::request::request_group::RequestGroup::new(
        GroupId::new(2),
        vec!["https://example.com/releases/my%20file.zip?token=ignored#fragment".to_string()],
        crate::request::request_group::DownloadOptions::default(),
    );
    group.set_total_length(4096);

    let mut result = DownloadResult::finished();
    result.fill_from_group(&group);

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "my file.zip");
}

#[cfg(feature = "bittorrent")]
#[test]
fn stopped_metadata_snapshot_matches_session_save_policy() {
    use crate::session::session_serializer::{
        serialize_groups_with_results, should_save_download_result,
    };

    let mut info = std::collections::BTreeMap::new();
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    info.insert(b"length".to_vec(), BencodeValue::Int(1));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"snapshot.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
    let mut torrent = std::collections::BTreeMap::new();
    torrent.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
    );
    torrent.insert(b"info".to_vec(), BencodeValue::Dict(info));
    let metadata = BencodeValue::Dict(torrent).encode();
    let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&metadata)
        .expect("fixture torrent metadata parses")
        .info_hash
        .as_hex();
    let group = crate::request::request_group::RequestGroup::new(
        GroupId::new(3),
        vec![format!("bt://{info_hash}")],
        crate::request::request_group::DownloadOptions::default(),
    );
    group.set_bt_metadata_data(metadata.to_vec());

    let cases = [
        (
            DownloadResultCode::Finished,
            Some(("force-save", false)),
            false,
        ),
        (
            DownloadResultCode::Finished,
            Some(("force-save", true)),
            true,
        ),
        (
            DownloadResultCode::Removed,
            Some(("force-save", false)),
            false,
        ),
        (
            DownloadResultCode::Removed,
            Some(("force-save", true)),
            true,
        ),
        (DownloadResultCode::InProgress, None, true),
        (
            DownloadResultCode::ResourceNotFound,
            Some(("save-not-found", false)),
            false,
        ),
        (
            DownloadResultCode::ResourceNotFound,
            Some(("save-not-found", true)),
            true,
        ),
        (
            DownloadResultCode::MaxFileNotFound,
            Some(("save-not-found", false)),
            false,
        ),
        (
            DownloadResultCode::MaxFileNotFound,
            Some(("save-not-found", true)),
            true,
        ),
        (DownloadResultCode::TimeOut, None, true),
    ];

    for (index, (code, option, expected_save)) in cases.into_iter().enumerate() {
        let snapshot = option
            .map(|(key, value)| {
                std::collections::HashMap::from([(key.to_string(), serde_json::Value::Bool(value))])
            })
            .unwrap_or_default();
        let mut result = DownloadResult::new(
            GroupId::new(10 + index as u64),
            DownloadStatus::Complete,
            code,
        );
        result.set_option_snapshot(Some(snapshot));
        result.fill_from_group(&group);

        assert_eq!(
            result.bt_metadata_data().is_some(),
            expected_save,
            "metadata retention for {code:?} with option {option:?}"
        );
        assert_eq!(
            should_save_download_result(&result),
            expected_save,
            "session-save policy for {code:?} with option {option:?}"
        );
        let serialized =
            serialize_groups_with_results(&[], &[result]).expect("session results serialize");
        assert_eq!(
            serialized.contains("aria2-rust-bt-metadata-data="),
            expected_save,
            "serialized metadata for {code:?} with option {option:?}"
        );
        if code == DownloadResultCode::Finished && expected_save {
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(
                    serialized
                        .split("aria2-rust-bt-metadata-data=")
                        .nth(1)
                        .expect("serialized torrent metadata option")
                        .split_whitespace()
                        .next()
                        .expect("base64 metadata value"),
                )
                .expect("serialized metadata is base64");
            let decoded: Vec<u8> =
                serde_json::from_slice(&decoded).expect("metadata descriptor is JSON bytes");
            assert_eq!(decoded, metadata);
        }
    }
}

#[test]
fn test_result_code_roundtrip() {
    for code in [
        DownloadResultCode::Finished,
        DownloadResultCode::TimeOut,
        DownloadResultCode::Removed,
        DownloadResultCode::InProgress,
        DownloadResultCode::ChecksumError,
    ] {
        assert_eq!(DownloadResultCode::from_code(code.as_code()), Some(code));
    }
}

#[test]
fn test_is_success() {
    assert!(DownloadResultCode::Finished.is_success());
    assert!(!DownloadResultCode::TimeOut.is_success());
}

#[test]
fn test_is_resumable() {
    assert!(DownloadResultCode::InProgress.is_resumable());
    assert!(!DownloadResultCode::TimeOut.is_resumable());
}

#[test]
fn test_is_user_stopped() {
    assert!(DownloadResultCode::Removed.is_user_stopped());
    assert!(!DownloadResultCode::InProgress.is_user_stopped());
}

#[test]
fn test_download_result_finished() {
    let r = DownloadResult::finished();
    assert_eq!(r.code, DownloadResultCode::Finished);
    assert_eq!(r.total_length, 0);
    assert!(r.followed_by.is_empty());
}

#[test]
fn test_download_result_removed() {
    let r = DownloadResult::removed();
    assert_eq!(r.code, DownloadResultCode::Removed);
}

#[test]
fn test_download_result_in_progress() {
    let r = DownloadResult::in_progress();
    assert_eq!(r.code, DownloadResultCode::InProgress);
}

#[test]
fn test_download_result_paused_has_no_rust_only_error_code() {
    let r = DownloadResult::paused();
    assert_eq!(r.status, DownloadStatus::Paused);
    assert_eq!(r.code, DownloadResultCode::UnknownError);
    assert_ne!(r.code.as_code(), 33);
}

#[test]
fn test_download_result_has_all_rpc_fields() {
    let r = DownloadResult::finished();
    // Verify all fields that RPC consumers expect are present.
    assert_eq!(r.gid_hex(), "0000000000000000");
    assert_eq!(r.total_length, 0);
    assert_eq!(r.completed_length, 0);
    assert_eq!(r.upload_length, 0);
    assert_eq!(r.download_speed, 0);
    assert_eq!(r.upload_speed, 0);
    assert_eq!(r.num_pieces, 0);
    assert_eq!(r.piece_length, 0);
    assert!(r.bitfield.is_empty());
    assert!(r.followed_by.is_empty());
    assert!(r.following.is_none());
    assert!(r.belongs_to.is_none());
    assert!(r.dir.is_empty());
    assert!(r.files.is_empty());
    assert!(r.info_hash.is_empty());
    assert!(r.attrs.is_empty());
    assert!(!r.in_memory_download);
    assert_eq!(r.session_download_length, 0);
    assert_eq!(r.session_time, 0);
}
