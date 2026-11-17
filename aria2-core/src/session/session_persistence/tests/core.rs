use super::*;

#[tokio::test]
async fn test_session_save_creates_files() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);

    let groups = create_test_groups(3);

    // Save state
    let saved_count = persistence
        .save_state(&groups)
        .await
        .expect("Save should succeed");

    assert_eq!(saved_count, 3, "Should save 3 commands");

    // Verify .aria2 files were created
    let entries: Vec<_> = fs::read_dir(&session_dir)
        .expect("Should read session dir")
        .filter_map(|e| e.ok())
        .collect();

    // Should have at least 3 .aria2 files + 1 options file
    let aria2_count = entries
        .iter()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "aria2")
                .unwrap_or(false)
        })
        .count();

    assert_eq!(aria2_count, 3, "Should have 3 .aria2 files");

    // Verify each file contains valid JSON with GID
    for entry in entries.iter().filter(|e| {
        e.path()
            .extension()
            .map(|ext| ext == "aria2")
            .unwrap_or(false)
    }) {
        let content = fs::read_to_string(entry.path()).expect("Should read file");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("Should be valid JSON");
        assert!(
            parsed.get("gid").is_some(),
            "Each .aria2 file should contain a GID field"
        );
    }

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_save_removes_stale_resume_files() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);
    let groups = create_test_groups(1);
    let resume_path = session_dir.join(format!(
        "{}.aria2",
        groups[0].recover().gid().to_hex_string()
    ));

    persistence.save_state(&groups).await.unwrap();
    assert!(resume_path.exists());

    let empty_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    persistence.save_state(&empty_groups).await.unwrap();

    assert!(
        !resume_path.exists(),
        "an empty authoritative snapshot must remove stale resume files"
    );

    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_generated_children_are_excluded_from_json_persistence() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);

    let parent = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2001),
        vec!["http://example.com/metadata.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let child = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2002),
        vec!["http://example.com/payload.bin".to_string()],
        DownloadOptions::default(),
    )));
    child.recover().set_belongs_to_gid(GroupId::new(2001));

    let saved = persistence.save_state(&[parent, child]).await.unwrap();
    assert_eq!(saved, 1);
    assert!(session_dir.join("00000000000007d1.aria2").exists());
    assert!(!session_dir.join("00000000000007d2.aria2").exists());

    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_load_restores_commands() {
    let session_dir = create_test_session_dir();
    let mut persistence = SessionPersistence::new(&session_dir);

    // Create and save original groups
    let original_groups = create_test_groups(2);
    let saved = persistence
        .save_state(&original_groups)
        .await
        .expect("Save should succeed");
    assert_eq!(saved, 2, "Should save 2 commands");

    // Load into empty groups vector
    let mut loaded_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let loaded = persistence
        .load_state(&mut loaded_groups)
        .await
        .expect("Load should succeed");

    assert_eq!(loaded, 2, "Should restore 2 commands");

    // Verify restored groups have URIs
    let mut found_uris: Vec<String> = Vec::new();
    for group_lock in &loaded_groups {
        let group = group_lock.recover();
        for uri in group.uris() {
            found_uris.push(uri.to_string());
        }
    }

    assert!(
        found_uris.iter().any(|u| u.contains("file0.bin")),
        "Should restore first file URI"
    );
    assert!(
        found_uris.iter().any(|u| u.contains("file1.bin")),
        "Should restore second file URI"
    );

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_restore_preserves_progress_pause_and_bt_state() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(0x456),
        vec!["bt://payload".to_string()],
        DownloadOptions {
            dir: Some(session_dir.to_string_lossy().into_owned()),
            out: Some("payload.bin".to_string()),
            checksum: Some(("sha-256".to_string(), "deadbeef".to_string())),
            ..Default::default()
        },
    )));
    group.recover().set_total_length(4096);
    group.recover().set_completed_length(1024);
    group.recover().set_uploaded_length(512);
    group.recover().set_bt_bitfield(Some(vec![0x80, 0x01]));
    group.recover().set_bt_metadata(
        8,
        512,
        "00112233445566778899aabbccddeeff00112233".to_string(),
    );
    group.recover_mut().pause().unwrap();

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
    let restored = restored[0].recover();
    assert_eq!(restored.total_length(), 4096);
    assert_eq!(restored.completed_length(), 1024);
    assert_eq!(restored.upload_length(), 512);
    assert_eq!(restored.get_bt_bitfield(), Some(vec![0x80, 0x01]));
    assert_eq!(restored.get_bt_num_pieces(), 8);
    assert_eq!(restored.get_bt_piece_length(), 512);
    assert_eq!(
        restored.get_bt_info_hash_hex().as_deref(),
        Some("00112233445566778899aabbccddeeff00112233")
    );
    assert!(restored.status().is_paused());
    assert_eq!(
        restored
            .options()
            .checksum
            .as_ref()
            .map(|(_, value)| value.as_str()),
        Some("deadbeef")
    );
    assert_eq!(
        restored.options().dir.as_deref(),
        Some(session_dir.to_string_lossy().as_ref())
    );
    assert_eq!(restored.options().out.as_deref(), Some("payload.bin"));

    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_save_empty_no_error() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);

    // Save empty groups list - should succeed without error
    let empty_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let result = persistence.save_state(&empty_groups).await;

    assert!(result.is_ok(), "Saving empty session should not error");
    let saved_count = result.unwrap();
    assert_eq!(saved_count, 0, "Empty session should report 0 saved");

    // Session directory should still exist (with options file at least)
    assert!(
        session_dir.exists(),
        "Session dir should be created even for empty save"
    );

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_corrupted_file_skipped_gracefully() {
    let session_dir = create_test_session_dir();
    fs::create_dir_all(&session_dir).expect("Session directory should exist");

    // Create a corrupted .aria2 file
    let corrupt_file = session_dir.join("corrupt-gid.aria2");
    fs::write(&corrupt_file, "THIS IS NOT VALID JSON {{{{").expect("Should write corrupt file");

    // Also create a valid .aria2 file
    let valid_file = session_dir.join("valid-gid.aria2");
    let valid_resume_data = ResumeData {
        gid: "abc123".to_string(),
        uris: vec![crate::engine::resume_data::UriState {
            uri: "http://example.com/valid-file.bin".to_string(),
            tried: true,
            used: false,
            last_result: None,
            speed_bytes_per_sec: None,
        }],
        total_length: 1024,
        completed_length: 512,
        uploaded_length: 0,
        bitfield: vec![],
        num_pieces: None,
        piece_length: None,
        status: "paused".to_string(),
        error_message: None,
        last_download_time: 0,
        created_at: 0,
        output_path: Some("/downloads/valid-file.bin".to_string()),
        checksum: None,
        options: std::collections::HashMap::new(),
        resume_offset: Some(512),
        bt_info_hash: None,
        bt_saved_metadata_path: None,
        metalink_data: None,
        metalink_file_index: None,
    };
    valid_resume_data
        .save_to_file(&valid_file)
        .expect("Should write valid file");

    let mut persistence = SessionPersistence::new(&session_dir);
    let mut loaded_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();

    // Load should succeed despite corrupt file
    let result = persistence.load_state(&mut loaded_groups).await;

    assert!(result.is_ok(), "Load should succeed despite corrupt file");
    let loaded_count = result.unwrap();
    assert_eq!(
        loaded_count, 1,
        "Should load 1 valid file (corrupt one skipped)"
    );

    // Verify the valid one was loaded correctly
    assert_eq!(loaded_groups.len(), 1, "Should have 1 restored group");

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_session_load_rejects_duplicate_gid() {
    let session_dir = create_test_session_dir();
    let first = ResumeData {
        gid: "2a".to_string(),
        uris: vec![crate::engine::resume_data::UriState {
            uri: "http://example.com/first.bin".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let second = ResumeData {
        gid: "2a".to_string(),
        uris: vec![crate::engine::resume_data::UriState {
            uri: "http://example.com/second.bin".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    first
        .save_to_file(&session_dir.join("first.aria2"))
        .unwrap();
    second
        .save_to_file(&session_dir.join("second.aria2"))
        .unwrap();

    let mut persistence = SessionPersistence::new(&session_dir);
    let mut groups = Vec::new();
    assert_eq!(persistence.load_state(&mut groups).await.unwrap(), 1);
    assert_eq!(groups[0].recover().gid(), GroupId::new(0x2a));

    let _ = fs::remove_dir_all(&session_dir);
}
