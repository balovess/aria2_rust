//! Tests for session persistence module.

use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::resume_data::ResumeData;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

/// Helper to create a temporary directory for tests
fn create_test_session_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % 1_000_000_000;
    let dir =
        std::env::temp_dir().join(format!("aria2_session_test_{}_{}", std::process::id(), ts));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("Failed to create test session directory");
    dir
}

/// Helper to create test RequestGroups
fn create_test_groups(count: usize) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
    let mut groups = Vec::new();
    for i in 0..count {
        let gid = GroupId::new(i as u64 + 1000);
        let uri = format!("http://example.com/file{}.bin", i);
        let options = DownloadOptions {
            dir: Some("/downloads".to_string()),
            split: Some(4),
            ..Default::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri],
            options,
        )));
        groups.push(group);
    }
    groups
}

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
            found_uris.push(uri.clone());
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

    // Create a corrupted .aria2 file
    let corrupt_file = session_dir.join("corrupt-gid.aria2");
    fs::write(&corrupt_file, "THIS IS NOT VALID JSON {{{{").expect("Should write corrupt file");

    // Also create a valid .aria2 file
    let valid_file = session_dir.join("valid-gid.aria2");
    let valid_resume_data = ResumeData {
        gid: "valid-gid-12345".to_string(),
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
async fn test_session_custom_interval() {
    let session_dir = create_test_session_dir();

    let persistence = SessionPersistence::new(&session_dir).with_interval(30);

    assert_eq!(
        persistence.auto_save_interval,
        Duration::from_secs(30),
        "Custom interval should be set"
    );

    // Test minimum interval enforcement
    let short_interval = SessionPersistence::new(&session_dir).with_interval(1);
    assert!(
        short_interval.auto_save_interval >= Duration::from_secs(10),
        "Interval should be at least 10 seconds"
    );

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
    use crate::http::cookie_storage::{CookieJar, JarCookie};

    let session_dir = create_test_session_dir();

    // Create original session with cookie jar
    let mut jar = CookieJar::new();
    jar.store(JarCookie::new("session_id", "abc123", "example.com"));
    jar.store(JarCookie::new("auth_token", "xyz789", "api.example.com"));

    let persistence_with_cookies = SessionPersistence::new(&session_dir).with_cookie_jar(jar);

    // Verify cookies are set
    assert!(
        persistence_with_cookies.cookie_jar().is_some(),
        "Cookie jar should be set"
    );
    assert_eq!(
        persistence_with_cookies.cookie_jar().unwrap().len(),
        2,
        "Should have 2 cookies before save"
    );

    // Save session (includes cookies)
    let groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let _saved = persistence_with_cookies.save_state(&groups).await.unwrap();

    // Verify cookies.json file was created
    let cookie_path = session_dir.join("cookies.json");
    assert!(
        cookie_path.exists(),
        "cookies.json file should exist after save"
    );

    // Load into new instance (without pre-set cookies)
    let mut persistence_new = SessionPersistence::new(&session_dir);
    let mut loaded_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = Vec::new();
    let _loaded = persistence_new
        .load_state(&mut loaded_groups)
        .await
        .unwrap();

    // Verify cookies were loaded
    assert!(
        persistence_new.cookie_jar().is_some(),
        "Cookie jar should exist after load"
    );
    let loaded_jar = persistence_new.cookie_jar().unwrap();
    assert_eq!(
        loaded_jar.len(),
        2,
        "Should have loaded 2 cookies from file"
    );

    // Verify specific cookies were preserved
    let example_cookies = loaded_jar.get_cookies_for_url("http://example.com/", false);
    assert_eq!(
        example_cookies.len(),
        1,
        "Should find 1 cookie for example.com"
    );
    assert_eq!(example_cookies[0].name, "session_id");
    assert_eq!(example_cookies[0].value, "abc123");

    let api_cookies = loaded_jar.get_cookies_for_url("http://api.example.com/api", false);
    assert_eq!(
        api_cookies.len(),
        2,
        "Should find 2 cookies for api.example.com (parent domain + exact)"
    );
    let auth_cookie = api_cookies
        .iter()
        .find(|c| c.name == "auth_token")
        .expect("Should find auth_token cookie");
    assert_eq!(auth_cookie.value, "xyz789");
    let session_cookie = api_cookies
        .iter()
        .find(|c| c.name == "session_id")
        .expect("Should find session_id cookie from parent domain");
    assert_eq!(session_cookie.value, "abc123");

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}

/// Test K2.4 #4: Auto-save with custom interval works correctly.
///
/// Verifies that non-default intervals are accepted and stored properly,
/// including enforcement of minimum interval requirement.
#[tokio::test]
async fn test_auto_save_with_custom_interval() {
    let session_dir = create_test_session_dir();

    // Test custom interval of 30 seconds
    let persistence_30s = SessionPersistence::new(&session_dir).with_interval(30);
    assert_eq!(
        persistence_30s.auto_save_interval,
        Duration::from_secs(30),
        "Custom 30s interval should be set"
    );

    // Test very short interval gets clamped to minimum (10 seconds)
    let persistence_too_short = SessionPersistence::new(&session_dir).with_interval(1);
    assert!(
        persistence_too_short.auto_save_interval >= Duration::from_secs(10),
        "Interval below minimum should be clamped to 10s"
    );

    // Test exact minimum interval
    let persistence_exact_min = SessionPersistence::new(&session_dir).with_interval(10);
    assert_eq!(
        persistence_exact_min.auto_save_interval,
        Duration::from_secs(10),
        "Exact minimum interval (10s) should be accepted"
    );

    // Test large interval
    let persistence_large = SessionPersistence::new(&session_dir).with_interval(300); // 5 minutes
    assert_eq!(
        persistence_large.auto_save_interval,
        Duration::from_secs(300),
        "Large interval (300s) should be accepted"
    );

    // Verify auto-save is enabled by default
    let persistence_default = SessionPersistence::new(&session_dir);
    assert!(
        persistence_default.auto_save_enabled,
        "Auto-save should be enabled by default"
    );
    assert_eq!(
        persistence_default.auto_save_interval,
        Duration::from_secs(DEFAULT_AUTO_SAVE_INTERVAL_SECS),
        "Default interval should be 60 seconds"
    );

    // Verify without_auto_save disables it
    let persistence_disabled = SessionPersistence::new(&session_dir).without_auto_save();
    assert!(
        !persistence_disabled.auto_save_enabled,
        "Auto-save should be disabled after without_auto_save()"
    );

    // Clean up
    let _ = fs::remove_dir_all(&session_dir);
}
