use super::*;

#[tokio::test]
async fn test_metadata_provenance_roundtrip_via_json_persistence() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(0xabc),
        vec!["bt://payload".to_string()],
        DownloadOptions::default(),
    )));
    group.recover().set_metadata_info(
        MetadataInfo::new(GroupId::new(0xabc), "https://example.test/meta.torrent")
            .with_metadata_path(session_dir.join("payload.torrent").to_string_lossy()),
    );

    assert_eq!(
        persistence.save_state(&[Arc::clone(&group)]).await.unwrap(),
        1
    );
    let mut restored = Vec::new();
    assert_eq!(
        SessionPersistence::new(&session_dir)
            .load_state(&mut restored)
            .await
            .unwrap(),
        1
    );
    let restored_info = restored[0].recover().metadata_info();
    assert!(restored_info.is_some());
    assert_eq!(
        restored_info.as_ref().and_then(|info| info.metadata_path()),
        Some(
            session_dir
                .join("payload.torrent")
                .to_string_lossy()
                .as_ref()
        )
    );

    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_follow_mode_variants_roundtrip_via_persistence() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);
    let modes = [
        (FollowMode::Follow, FollowMode::Disabled),
        (FollowMode::Disabled, FollowMode::Follow),
        (FollowMode::Memory, FollowMode::Memory),
    ];
    let groups = modes
        .iter()
        .enumerate()
        .map(|(index, &(follow_torrent, follow_metalink))| {
            Arc::new(std::sync::RwLock::new(RequestGroup::new(
                GroupId::new(0x400 + index as u64),
                vec![format!("https://example.test/metadata-{index}.bin")],
                DownloadOptions {
                    follow_torrent: Some(follow_torrent),
                    follow_metalink: Some(follow_metalink),
                    ..Default::default()
                },
            )))
        })
        .collect::<Vec<_>>();

    assert_eq!(persistence.save_state(&groups).await.unwrap(), modes.len());

    let mut restored = Vec::new();
    assert_eq!(
        SessionPersistence::new(&session_dir)
            .load_state(&mut restored)
            .await
            .unwrap(),
        modes.len()
    );

    for (index, &(follow_torrent, follow_metalink)) in modes.iter().enumerate() {
        let group = restored
            .iter()
            .find(|group| group.recover().gid() == GroupId::new(0x400 + index as u64))
            .expect("follow-mode group should be restored");
        let options = group.recover().options().clone();
        assert_eq!(options.follow_torrent, Some(follow_torrent));
        assert_eq!(options.follow_metalink, Some(follow_metalink));
    }

    let _ = fs::remove_dir_all(&session_dir);
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_metalink_torrent_graph_is_rebuilt_after_session_load() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);
    let metadata_path = session_dir.join("payload.torrent");
    let payload = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(0x222),
        vec!["bt://metadata".to_string()],
        DownloadOptions {
            dir: Some(session_dir.to_string_lossy().into_owned()),
            out: Some("payload.bin".to_string()),
            ..Default::default()
        },
    )));
    payload.recover().set_metadata_info(
        MetadataInfo::new(GroupId::new(0x111), "https://example.test/payload.torrent")
            .with_metadata_path(metadata_path.to_string_lossy()),
    );

    assert_eq!(
        persistence
            .save_state(&[Arc::clone(&payload)])
            .await
            .unwrap(),
        1
    );

    let mut restored = Vec::new();
    assert_eq!(
        SessionPersistence::new(&session_dir)
            .load_state(&mut restored)
            .await
            .unwrap(),
        1
    );
    assert_eq!(restored.len(), 2);

    let metadata = restored
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x111))
        .expect("metadata prerequisite should be restored");
    assert_eq!(
        metadata
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["https://example.test/payload.torrent"]
    );
    assert_eq!(
        metadata.recover().belongs_to_gid(),
        Some(GroupId::new(0x222))
    );

    let restored_payload = restored
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x222))
        .expect("payload should be restored");
    assert!(!restored_payload.recover().is_dependency_resolved());
    assert_eq!(
        restored_payload.recover().metadata_info().unwrap().gid(),
        Some(GroupId::new(0x111))
    );

    let _ = fs::remove_dir_all(&session_dir);
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_json_session_roundtrip_preserves_metalink_graph_descriptor() {
    let session_dir = create_test_session_dir();
    let options = DownloadOptions {
        dir: Some(session_dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let graph = crate::engine::metalink::request_graph::MetalinkRequestGraph::new_memory_with_fallback_and_mappings(
        "https://example.test/payload.torrent",
        "payload.bin",
        &options,
        GroupId::new(0x501),
        GroupId::new(0x502),
        vec!["https://mirror.example.test/payload.bin".to_string()],
        vec![crate::request::request_group::BtFileMapping {
            original_name: "payload.bin".to_string(),
            path: session_dir.join("payload.bin").to_string_lossy().into_owned(),
            uris: vec!["https://mirror.example.test/payload.bin".to_string()],
            max_connection_per_server: 3,
            unique_protocol: true,
        }],
    )
    .unwrap();
    let torrent_data = {
        let mut data =
            b"d8:announce28:http://tracker.test/announce4:infod6:lengthi0e4:name4:test12:piece lengthi16384e6:pieces20:".to_vec();
        data.extend_from_slice(&[
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ]);
        data.extend_from_slice(b"ee");
        data
    };
    let torrent_len = torrent_data.len();
    graph
        .payload
        .recover()
        .set_bt_metadata_data(torrent_data.clone());

    let persistence = SessionPersistence::new(&session_dir);
    assert_eq!(
        persistence
            .save_state(&[Arc::clone(&graph.payload)])
            .await
            .unwrap(),
        1
    );

    let mut restored = Vec::new();
    assert_eq!(
        SessionPersistence::new(&session_dir)
            .load_state(&mut restored)
            .await
            .unwrap(),
        1
    );
    assert_eq!(restored.len(), 2);

    let payload = restored
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x502))
        .expect("payload should be restored");
    let descriptor = payload
        .recover()
        .bt_dependency_descriptor()
        .expect("BT dependency descriptor should be rebuilt");
    assert!(descriptor.0);
    assert_eq!(
        descriptor.1,
        vec!["https://mirror.example.test/payload.bin"]
    );
    assert_eq!(descriptor.2.len(), 1);
    assert_eq!(descriptor.2[0].max_connection_per_server, 3);
    assert!(descriptor.2[0].unique_protocol);
    assert_eq!(payload.recover().bt_metadata_data(), Some(torrent_data));

    let metadata = restored
        .iter()
        .find(|group| group.recover().gid() == GroupId::new(0x501))
        .expect("metadata source should be restored");
    assert!(metadata.recover().is_in_memory_download());
    assert_eq!(
        metadata.recover().in_memory_data().map(|data| data.len()),
        Some(torrent_len)
    );

    let _ = fs::remove_dir_all(&session_dir);
}
