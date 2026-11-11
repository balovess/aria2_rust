use super::*;

#[tokio::test]
async fn test_session_load_nonexistent_dir_returns_zero() {
    let nonexistent_dir =
        PathBuf::from("/tmp/aria2_nonexistent_test_dir_that_should_not_exist_12345");
    let mut persistence = SessionPersistence::new(&nonexistent_dir);

    let mut groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let result = persistence.load_state(&mut groups).await;

    assert!(result.is_ok(), "Nonexistent dir should return Ok");
    assert_eq!(result.unwrap(), 0, "Nonexistent dir should return 0 loaded");
    assert!(groups.is_empty(), "No groups should be added");
}

#[tokio::test]
async fn test_session_cleanup_removes_all_files() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);

    // Create some files
    let groups = create_test_groups(2);
    let _ = persistence.save_state(&groups).await.unwrap();

    // Verify files exist
    assert!(
        session_dir.exists(),
        "Session dir should exist before cleanup"
    );

    // Cleanup
    persistence.cleanup().await.expect("Cleanup should succeed");

    // Verify directory is empty or removed
    if session_dir.exists() {
        let remaining: Vec<_> = fs::read_dir(&session_dir)
            .expect("Should read dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            remaining.is_empty(),
            "All files should be removed after cleanup"
        );
    }

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

#[tokio::test]
async fn test_resume_data_roundtrip_via_persistence() {
    let session_dir = create_test_session_dir();
    let mut persistence = SessionPersistence::new(&session_dir);

    // Create a group with specific properties
    let gid = GroupId::new(0xDEADBEEF);
    let options = DownloadOptions {
        dir: Some("/test/downloads".to_string()),
        out: Some("special_file.iso".to_string()),
        split: Some(16),
        ..Default::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec!["http://example.com/special_file.iso".to_string()],
        options,
    )));

    // Set some progress
    {
        let g = group.recover_mut();
        g.set_total_length_atomic(10485760); // 10MB
        g.set_completed_length(5242880); // 5MB
    }

    // Save
    let saved = persistence.save_state(&[group]).await.unwrap();
    assert_eq!(saved, 1);

    // Load back
    let mut loaded: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let loaded_count = persistence.load_state(&mut loaded).await.unwrap();
    assert_eq!(loaded_count, 1);

    // Verify the loaded group has correct URIs
    let restored = loaded[0].read().unwrap();
    let uris = restored.uris();
    assert_eq!(uris.len(), 1);
    assert!(uris[0].contains("special_file.iso"));

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

// =====================================================================
// K2.4 — New Tests for Session Enhancements
// =====================================================================

/// Test K2.4 #1: Selective save of active downloads only.
///
/// Creates a mix of active and completed downloads, then verifies that
/// save_active_only() only persists the active/waiting ones.
#[tokio::test]
async fn test_selective_save_active_only() {
    let session_dir = create_test_session_dir();
    let persistence = SessionPersistence::new(&session_dir);

    // Create groups with different statuses
    let mut groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();

    // Active download (should be saved)
    let active_gid = GroupId::new(1001);
    let active_group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        active_gid,
        vec!["http://example.com/active.bin".to_string()],
        DownloadOptions::default(),
    )));
    {
        let mut g = active_group.recover_mut();
        g.start().unwrap(); // Set to Active status
    }
    groups.push(active_group);

    // Waiting download (should be saved)
    let waiting_gid = GroupId::new(1002);
    let waiting_group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        waiting_gid,
        vec!["http://example.com/waiting.bin".to_string()],
        DownloadOptions::default(),
    )));
    // Waiting is default status, no need to change
    groups.push(waiting_group);

    // Completed download (should NOT be saved)
    let complete_gid = GroupId::new(1003);
    let complete_group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        complete_gid,
        vec!["http://example.com/complete.bin".to_string()],
        DownloadOptions::default(),
    )));
    {
        let mut g = complete_group.recover_mut();
        g.complete().unwrap(); // Set to Complete status
    }
    groups.push(complete_group);

    // Save only active/waiting
    let saved_count = persistence.save_active_only(&groups).await.unwrap();

    // Should save exactly 2 (active + waiting)
    assert_eq!(
        saved_count, 2,
        "save_active_only should save only active and waiting downloads"
    );

    // Verify files on disk - should have 2 .aria2 files
    let entries: Vec<_> = fs::read_dir(&session_dir)
        .expect("Should read session dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "aria2")
                .unwrap_or(false)
        })
        .collect();

    assert_eq!(
        entries.len(),
        2,
        "Should have exactly 2 .aria2 files for active+waiting"
    );

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

/// Test K2.4 #2: DHT snapshot roundtrip preserves data.
///
/// Creates a DhtStateSnapshot with sample data, serializes it to JSON,
/// then deserializes and verifies all fields are preserved correctly.
#[test]
fn test_dht_snapshot_roundtrip() {
    use crate::session::session_persistence::{DhtNodeInfo, DhtStateSnapshot};

    // Create original snapshot with data
    let node1 = DhtNodeInfo {
        id: [1u8; 20],
        addr: "192.168.1.100:6881".to_string(),
        last_seen_epoch_secs: 1700000000,
    };

    let node2 = DhtNodeInfo {
        id: [2u8; 20],
        addr: "10.0.0.5:6881".to_string(),
        last_seen_epoch_secs: 1700000100,
    };

    let token_secret: [u8; 20] = [0xAB; 20];

    let original = DhtStateSnapshot::new(vec![node1, node2], token_secret, Some(1699999000));

    // Verify initial state
    assert_eq!(original.total_nodes, 2);
    assert_eq!(original.nodes.len(), 2);
    assert!(original.last_bootstrap_epoch_secs.is_some());

    // Serialize to JSON
    let json = original
        .to_json_string()
        .expect("Serialization should succeed");
    assert!(!json.is_empty(), "JSON output should not be empty");
    assert!(
        json.contains("192.168.1.100"),
        "JSON should contain first node address"
    );
    assert!(
        json.contains("10.0.0.5"),
        "JSON should contain second node address"
    );

    // Deserialize from JSON
    let restored =
        DhtStateSnapshot::from_json_string(&json).expect("Deserialization should succeed");

    // Verify all fields match
    assert_eq!(restored.total_nodes, 2, "total_nodes should be preserved");
    assert_eq!(restored.nodes.len(), 2, "nodes count should be preserved");
    assert_eq!(
        restored.token_secret, token_secret,
        "token_secret should be preserved"
    );
    assert_eq!(
        restored.last_bootstrap_epoch_secs,
        Some(1699999000),
        "last_bootstrap_epoch_secs should be preserved"
    );

    // Verify individual node data
    assert_eq!(
        restored.nodes[0].id, [1u8; 20],
        "First node ID should match"
    );
    assert_eq!(
        restored.nodes[0].addr, "192.168.1.100:6881",
        "First node address should match"
    );
    assert_eq!(
        restored.nodes[0].last_seen_epoch_secs, 1700000000,
        "First node timestamp should match"
    );

    assert_eq!(
        restored.nodes[1].id, [2u8; 20],
        "Second node ID should match"
    );
    assert_eq!(
        restored.nodes[1].addr, "10.0.0.5:6881",
        "Second node address should match"
    );

    // Test empty snapshot
    let empty = DhtStateSnapshot::empty();
    assert_eq!(empty.total_nodes, 0, "Empty snapshot should have 0 nodes");
    assert!(
        empty.nodes.is_empty(),
        "Empty snapshot should have no nodes"
    );

    let empty_json = empty
        .to_json_string()
        .expect("Empty serialization should succeed");
    let empty_restored =
        DhtStateSnapshot::from_json_string(&empty_json).expect("Empty deserialization should work");
    assert_eq!(
        empty_restored.total_nodes, 0,
        "Restored empty should still be empty"
    );
}

/// Test K2.4 #3: Cookie persistence integration - cookies survive save/load cycle.
///
/// Creates a SessionPersistence with cookies, saves state, loads it into
/// a new instance, and verifies cookies are preserved.
#[tokio::test]
async fn test_cookie_persist_integration() {
    use crate::http::cookie::CookieStorage;

    let session_dir = create_test_session_dir();

    let storage = std::sync::Arc::new(CookieStorage::new());
    storage.parse_and_store("session_id=abc123; Path=/", "example.com", "/");
    storage.parse_and_store("auth_token=xyz789; Path=/", "api.example.com", "/");
    let persistence_with_cookies =
        SessionPersistence::new(&session_dir).with_cookie_storage(storage);

    // Save session (includes cookies)
    let groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let _saved = persistence_with_cookies.save_state(&groups).await.unwrap();

    // Verify the canonical Netscape cookie file was created
    let cookie_path = session_dir.join("cookies.txt");
    assert!(
        cookie_path.exists(),
        "cookies.txt file should exist after save"
    );

    // Load into new instance with an empty canonical storage
    let loaded_storage = std::sync::Arc::new(CookieStorage::new());
    let mut persistence_new =
        SessionPersistence::new(&session_dir).with_cookie_storage(loaded_storage.clone());
    let mut loaded_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let _loaded = persistence_new
        .load_state(&mut loaded_groups)
        .await
        .unwrap();

    let example_header = loaded_storage.to_header_string("example.com", "/", false);
    assert_eq!(example_header, "session_id=abc123");
    let api_header = loaded_storage.to_header_string("api.example.com", "/api", false);
    assert!(api_header.contains("auth_token=xyz789"));

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}
