#[cfg(feature = "bittorrent")]
#[test]
fn add_torrent_prepares_metadata_before_registration() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let mut torrent = b"d8:announce28:http://tracker.test/announce4:infod6:lengthi1e4:name8:file.bin12:piece lengthi1e6:pieces20:".to_vec();
    torrent.extend_from_slice(&[0; 20]);
    torrent.extend_from_slice(b"ee");

    let handle = manager
        .add_torrent(
            torrent,
            vec!["https://example.test/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .expect("valid torrent submission");

    assert!(handle.status_snapshot().is_some());
    let files = handle.get_files().expect("prepared file metadata");
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::path::Path::new(&files[0].path)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("file.bin")
    );
    assert_eq!(group_man.count(), 1);
}

#[cfg(feature = "bittorrent")]
#[test]
fn add_torrent_rejects_invalid_data_without_registration() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);

    let result = manager.add_torrent(vec![1, 2, 3], Vec::new(), DownloadOptions::default());

    assert!(
        matches!(
            result,
            Err(DownloadManagerError::Preparation(
                Aria2Error::BittorrentParse(_)
            ))
        ),
        "invalid torrent should retain the BitTorrent parse error: {:?}",
        result.as_ref().err()
    );
    assert_eq!(group_man.count(), 0);
}

#[cfg(feature = "metalink")]
#[test]
fn add_metalink_returns_handles_for_resource_groups() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><url>https://example.test/file.bin</url></file></metalink>"#;

    let handles = manager
        .add_metalink(data.to_vec(), DownloadOptions::default())
        .expect("valid Metalink submission");

    assert_eq!(handles.len(), 1);
    assert!(handles[0].status_snapshot().is_some());
    assert_eq!(group_man.count(), 1);
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn add_metalink_returns_metadata_and_payload_handles_for_torrent_metaurl() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><metaurl mediatype="torrent">https://example.test/file.torrent</metaurl></file></metalink>"#;

    let handles = manager
        .add_metalink(data.to_vec(), DownloadOptions::default())
        .expect("valid torrent Metalink submission");

    assert_eq!(handles.len(), 2);
    assert_ne!(handles[0].gid(), handles[1].gid());
    assert!(
        handles
            .iter()
            .all(|handle| handle.status_snapshot().is_some())
    );
    assert_eq!(group_man.count(), 2);
}
