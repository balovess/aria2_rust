#[allow(unused_imports)]
use std::sync::Arc;

#[allow(unused_imports)]
use super::group::RequestGroup;
#[allow(unused_imports)]
use super::group_id::GroupId;
#[allow(unused_imports)]
use super::options::DownloadOptions;
#[allow(unused_imports)]
use crate::download::DownloadContext;

#[test]
fn test_connection_contexts_are_deduplicated_and_reset() {
    use std::net::SocketAddr;

    let group = RequestGroup::new(GroupId::new(99), Vec::new(), DownloadOptions::default());
    let first: SocketAddr = "192.0.2.1:80".parse().unwrap();
    let second: SocketAddr = "192.0.2.2:80".parse().unwrap();
    let first_context = crate::network::ConnectionContext::new("example.test", 80, first);
    group.set_connection_context(first_context.clone());
    group.set_connection_context(first_context);
    group.set_connection_context(crate::network::ConnectionContext::new(
        "example.test",
        80,
        second,
    ));
    assert_eq!(group.connection_contexts().len(), 2);
    group.clear_connection_contexts();
    assert!(group.connection_contexts().is_empty());
}

#[test]
fn test_followed_by_gids_are_idempotent() {
    let group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file.zip".to_string()],
        DownloadOptions::default(),
    );
    let child = GroupId::new(2);
    group.add_followed_by_gid(child);
    group.add_followed_by_gid(child);
    assert_eq!(group.followed_by_gids(), vec![child]);
}

#[test]
fn test_child_relationship_fields_are_preserved() {
    let group = RequestGroup::new(GroupId::new(2), Vec::new(), DownloadOptions::default());
    let parent = GroupId::new(1);
    group.set_following_gid(parent);
    group.set_belongs_to_gid(parent);
    assert_eq!(group.following_gid(), Some(parent));
    assert_eq!(group.belongs_to_gid(), Some(parent));
}

#[test]
fn test_metadata_info_is_preserved_independently_of_parent_link() {
    let group = RequestGroup::new(GroupId::new(2), Vec::new(), DownloadOptions::default());
    group.set_metadata_info(super::metadata_info::MetadataInfo::new(
        GroupId::new(9),
        "https://example.test/metadata.torrent",
    ));

    let info = group
        .metadata_info()
        .expect("metadata info should be attached");
    assert_eq!(info.gid(), Some(GroupId::new(9)));
    assert_eq!(info.uri(), "https://example.test/metadata.torrent");
    assert!(group.belongs_to_gid().is_none());
}

#[test]
fn test_request_group_progress_fields_default() {
    // New RequestGroup should have all zeros/None defaults for progress fields
    let group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file.zip".to_string()],
        DownloadOptions::default(),
    );

    // Verify all atomic fields default to 0
    assert_eq!(
        group.get_completed_length(),
        0,
        "completed_length_atomic should default to 0"
    );
    assert_eq!(
        group.get_total_length_atomic(),
        0,
        "total_length_atomic should default to 0"
    );
    assert_eq!(
        group.get_uploaded_length(),
        0,
        "uploaded_length should default to 0"
    );
    assert_eq!(
        group.get_download_speed_cached(),
        0,
        "download_speed_cached should default to 0"
    );
    assert_eq!(
        group.get_upload_speed_cached(),
        0,
        "upload_speed_cached should default to 0"
    );
}

#[test]
fn test_set_get_completed_length() {
    let group = RequestGroup::new(
        GroupId::new(2),
        vec!["http://test.com/file.bin".to_string()],
        DownloadOptions::default(),
    );

    // Test set/get roundtrip
    group.set_completed_length(1024);
    assert_eq!(
        group.get_completed_length(),
        1024,
        "Should return 1024 after setting"
    );

    // Test update to different value
    group.set_completed_length(2048);
    assert_eq!(
        group.get_completed_length(),
        2048,
        "Should return 2048 after update"
    );

    // Test large value
    group.set_completed_length(u64::MAX);
    assert_eq!(
        group.get_completed_length(),
        u64::MAX,
        "Should handle u64::MAX"
    );

    // Test zero
    group.set_completed_length(0);
    assert_eq!(group.get_completed_length(), 0, "Should handle 0");
}

#[test]
fn test_validate_total_length() {
    let group = RequestGroup::new(
        GroupId(1),
        vec!["ftp://example/file".into()],
        DownloadOptions::default(),
    );
    assert!(group.validate_total_length(0, 1024).is_ok());
    assert!(group.validate_total_length(1024, 1024).is_ok());
    assert!(group.validate_total_length(1024, 2048).is_err());
}

#[test]
fn test_set_get_total_length() {
    let group = RequestGroup::new(
        GroupId::new(3),
        vec!["http://example.com/large.iso".to_string()],
        DownloadOptions::default(),
    );

    // Test set/get roundtrip
    group.set_total_length_atomic(1048576); // 1MB
    assert_eq!(
        group.get_total_length_atomic(),
        1048576,
        "Should return 1MB after setting"
    );

    // Test update
    group.set_total_length_atomic(1073741824); // 1GB
    assert_eq!(
        group.get_total_length_atomic(),
        1073741824,
        "Should return 1GB after update"
    );
}

#[test]
fn test_set_get_bt_bitfield() {
    let group = RequestGroup::new(
        GroupId::new(4),
        vec!["magnet:?xt=urn:btih:abc123".to_string()],
        DownloadOptions::default(),
    );

    // Default should be None
    let bf = group.get_bt_bitfield();
    assert!(bf.is_none(), "bt_bitfield should default to None");

    // Set and retrieve bitfield
    let test_bitfield = vec![0xFF, 0xF0, 0x0F];
    group.set_bt_bitfield(Some(test_bitfield.clone()));
    let retrieved = group.get_bt_bitfield();
    assert!(
        retrieved.is_some(),
        "bt_bitfield should be Some after setting"
    );
    assert_eq!(
        retrieved.unwrap(),
        test_bitfield,
        "bitfield should match what was set"
    );

    // Set back to None
    group.set_bt_bitfield(None);
    let bf_none = group.get_bt_bitfield();
    assert!(
        bf_none.is_none(),
        "bt_bitfield should be None after clearing"
    );

    // Test with empty bitfield
    group.set_bt_bitfield(Some(vec![]));
    let empty_bf = group.get_bt_bitfield();
    assert!(empty_bf.is_some(), "empty bitfield should still be Some");
    assert!(empty_bf.unwrap().is_empty(), "bitfield should be empty vec");
}

#[tokio::test]
async fn test_concurrent_access() {
    let group = Arc::new(RequestGroup::new(
        GroupId::new(5),
        vec!["http://load.test/file.dat".to_string()],
        DownloadOptions::default(),
    ));

    // Spawn multiple tasks that read/write progress concurrently
    let mut handles = Vec::new();

    for i in 0..10 {
        let g = Arc::clone(&group);
        handles.push(tokio::spawn(async move {
            // Write progress
            g.set_completed_length(i * 100);
            g.set_total_length_atomic(10000);
            g.set_uploaded_length(i * 10);
            g.set_download_speed_cached(i * 1000);

            // Read progress (should not deadlock)
            let _cl = g.get_completed_length();
            let _tl = g.get_total_length_atomic();
            let _ul = g.get_uploaded_length();
            let _ds = g.get_download_speed_cached();

            // Occasionally write bitfield (sync)
            if i % 3 == 0 {
                let bf = vec![i as u8; 8];
                g.set_bt_bitfield(Some(bf));
                let _retrieved = g.get_bt_bitfield();
            }

            // Small delay to increase chance of race conditions
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }));
    }

    // Wait for all tasks to complete without deadlock
    for handle in handles {
        handle.await.expect("Task should complete without panic");
    }

    // Verify final state is consistent
    let final_cl = group.get_completed_length();
    let final_tl = group.get_total_length_atomic();
    let final_ul = group.get_uploaded_length();
    let final_ds = group.get_download_speed_cached();

    // Values should be from one of the concurrent writers (we don't know which)
    assert!(final_cl <= 900, "completed_length should be <= 900");
    assert_eq!(final_tl, 10000, "total_length should be 10000");
    assert!(final_ul <= 90, "uploaded_length should be <= 90");
    assert!(final_ds <= 9000, "download_speed should be <= 9000");
}

#[test]
fn test_set_get_uploaded_length() {
    let group = RequestGroup::new(
        GroupId::new(6),
        vec!["http://seed.test/file.torrent".to_string()],
        DownloadOptions::default(),
    );

    // Test default
    assert_eq!(group.get_uploaded_length(), 0);

    // Test set/get
    group.set_uploaded_length(512);
    assert_eq!(group.get_uploaded_length(), 512);

    // Test large value
    group.set_uploaded_length(u64::MAX / 2);
    assert_eq!(group.get_uploaded_length(), u64::MAX / 2);
}

#[test]
fn test_set_get_download_speed_cached() {
    let group = RequestGroup::new(
        GroupId::new(7),
        vec!["http://speed.test/large.file".to_string()],
        DownloadOptions::default(),
    );

    // Test default
    assert_eq!(group.get_download_speed_cached(), 0);

    // Test realistic download speed (e.g., 5 MB/s = 5242880 bytes/s)
    group.set_download_speed_cached(5242880);
    assert_eq!(group.get_download_speed_cached(), 5242880);

    // Test speed update (simulating periodic updates)
    group.set_download_speed_cached(10485760); // 10 MB/s
    assert_eq!(group.get_download_speed_cached(), 10485760);
}

#[test]
fn test_download_options_choking_config_defaults() {
    // New DownloadOptions should have None for choking algorithm fields (opt-in)
    let opts = DownloadOptions::default();

    assert!(
        opts.bt_max_upload_slots.is_none(),
        "bt_max_upload_slots should default to None"
    );
    assert!(
        opts.bt_optimistic_unchoke_interval.is_none(),
        "bt_optimistic_unchoke_interval should default to None"
    );
    assert!(
        opts.bt_snubbed_timeout.is_none(),
        "bt_snubbed_timeout should default to None"
    );
}

#[test]
fn test_download_options_choking_config_custom() {
    // Verify that custom choking config values can be set
    let opts = DownloadOptions {
        bt_max_upload_slots: Some(8),
        bt_optimistic_unchoke_interval: Some(15),
        bt_snubbed_timeout: Some(45),
        ..DownloadOptions::default()
    };

    assert_eq!(opts.bt_max_upload_slots, Some(8));
    assert_eq!(opts.bt_optimistic_unchoke_interval, Some(15));
    assert_eq!(opts.bt_snubbed_timeout, Some(45));
}

#[test]
fn test_download_options_choking_config_clone() {
    // Verify choking config fields are preserved through Clone
    let opts = DownloadOptions {
        bt_max_upload_slots: Some(6),
        bt_optimistic_unchoke_interval: Some(20),
        bt_snubbed_timeout: Some(90),
        ..DownloadOptions::default()
    };

    let cloned = opts.clone();

    assert_eq!(cloned.bt_max_upload_slots, Some(6));
    assert_eq!(cloned.bt_optimistic_unchoke_interval, Some(20));
    assert_eq!(cloned.bt_snubbed_timeout, Some(90));
}

// ==================== BT Metadata Tests ====================

#[test]
fn test_bt_metadata_defaults() {
    let group = RequestGroup::new(
        GroupId::new(8),
        vec!["http://example.com/file.zip".to_string()],
        DownloadOptions::default(),
    );

    // Non-BT downloads should have 0/None defaults
    assert_eq!(
        group.get_bt_num_pieces(),
        0,
        "bt_num_pieces should default to 0"
    );
    assert_eq!(
        group.get_bt_piece_length(),
        0,
        "bt_piece_length should default to 0"
    );
    assert_eq!(
        group.get_bt_info_hash_hex(),
        None,
        "bt_info_hash_hex should default to None"
    );
}

#[test]
fn test_set_bt_metadata() {
    let group = RequestGroup::new(
        GroupId::new(9),
        vec!["magnet:?xt=urn:btih:abc123def456".to_string()],
        DownloadOptions::default(),
    );

    // Set BT metadata
    group.set_bt_metadata(
        100,
        262144,
        "abc123def456789012345678901234567890abcd".to_string(),
    );

    // Verify values
    assert_eq!(group.get_bt_num_pieces(), 100);
    assert_eq!(group.get_bt_piece_length(), 262144); // 256KB
    assert_eq!(
        group.get_bt_info_hash_hex(),
        Some("abc123def456789012345678901234567890abcd".to_string())
    );
}

#[test]
fn test_bt_metadata_update() {
    let group = RequestGroup::new(
        GroupId::new(10),
        vec!["bt://test.torrent".to_string()],
        DownloadOptions::default(),
    );

    // Initial set
    group.set_bt_metadata(50, 16384, "first_hash".to_string());
    assert_eq!(group.get_bt_num_pieces(), 50);

    // Update with new values
    group.set_bt_metadata(200, 524288, "updated_hash".to_string());
    assert_eq!(group.get_bt_num_pieces(), 200);
    assert_eq!(group.get_bt_piece_length(), 524288);
    assert_eq!(
        group.get_bt_info_hash_hex(),
        Some("updated_hash".to_string())
    );
}

#[test]
fn test_bt_info_hash_hex() {
    let group = RequestGroup::new(
        GroupId::new(11),
        vec!["magnet:?xt=urn:btih:test".to_string()],
        DownloadOptions::default(),
    );

    // Set via blocking method
    group.set_bt_metadata(10, 1024, "async_test_hash".to_string());

    // Read via sync method
    let hash = group.get_bt_info_hash_hex();
    assert_eq!(hash, Some("async_test_hash".to_string()));
}

#[test]
fn test_update_option_new_runtime_changeable() {
    let gid = GroupId::new(1);
    let uris = vec!["http://example.com/file".to_string()];
    let mut group = RequestGroup::new(gid, uris, DownloadOptions::default());

    // max-connection-per-server
    assert!(group.update_option("max-connection-per-server", serde_json::json!(4)));
    assert_eq!(group.options().max_connection_per_server, Some(4));

    // bt-max-upload-slots
    assert!(group.update_option("bt-max-upload-slots", serde_json::json!(8)));
    assert_eq!(group.options().bt_max_upload_slots, Some(8));

    // bt-snubbed-timeout
    assert!(group.update_option("bt-snubbed-timeout", serde_json::json!(120)));
    assert_eq!(group.options().bt_snubbed_timeout, Some(120));

    // bt-optimistic-unchoke-interval
    assert!(group.update_option("bt-optimistic-unchoke-interval", serde_json::json!(45)));
    assert_eq!(group.options().bt_optimistic_unchoke_interval, Some(45));

    // bt-endgame-threshold
    assert!(group.update_option("bt-endgame-threshold", serde_json::json!(50)));
    assert_eq!(group.options().bt_endgame_threshold, 50);

    // seed-time
    assert!(group.update_option("seed-time", serde_json::json!(3600)));
    assert_eq!(group.options().seed_time, Some(3600.0));

    // seed-ratio
    assert!(group.update_option("seed-ratio", serde_json::json!(2.0)));
    assert_eq!(group.options().seed_ratio, Some(2.0));

    // Unknown option returns false
    assert!(!group.update_option("unknown-option", serde_json::json!(1)));
}

/// Verify that `is_removed()` correctly reflects the group's Removed
/// status, and that it is non-blocking (does not deadlock when the
/// status lock is contended).
#[test]
fn test_is_removed_reflects_status() {
    let mut group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file".to_string()],
        DownloadOptions::default(),
    );

    // Fresh group is in Waiting state, not Removed.
    assert!(!group.is_removed(), "fresh group should not be removed");

    // Mark as Removed (as RequestGroupMan::remove_group does).
    group.remove().unwrap();
    assert!(
        group.is_removed(),
        "is_removed() must return true after group.remove()"
    );
}

/// `is_removed()` must be safe to call while a write lock on the status
/// is held elsewhere. It uses `try_read` internally, so it should return
/// `false` (not deadlock, not block) when the lock is contended by a writer.
#[test]
fn test_is_removed_returns_false_when_write_locked() {
    let mut group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file".to_string()],
        DownloadOptions::default(),
    );
    group.remove().unwrap();

    // Hold a write lock on the inner status to simulate contention.
    // This blocks try_read() on the same lock.
    let _guard = group.status.write().unwrap();
    // is_removed() uses try_read(), which fails when a write lock is held.
    // It must return false (not block, not panic).
    assert!(
        !group.is_removed(),
        "is_removed() should return false when the status write lock is held (try_read fails)"
    );
    // Lock released when _guard drops.
}

// -----------------------------------------------------------------------
// DownloadContext integration tests
// -----------------------------------------------------------------------

#[test]
fn test_download_context_default_is_none() {
    let group = RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/file.zip".to_string()],
        DownloadOptions::default(),
    );
    assert!(
        group.get_download_context().is_none(),
        "download_context should default to None for non-BT downloads"
    );
}

#[test]
fn test_set_and_get_download_context() {
    let group = RequestGroup::new(
        GroupId::new(2),
        vec!["bt://test".to_string()],
        DownloadOptions::default(),
    );

    let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
    group.set_download_context(Arc::clone(&ctx));

    let retrieved = group.get_download_context();
    assert!(retrieved.is_some(), "download_context should be set");
    assert!(
        Arc::ptr_eq(&retrieved.unwrap(), &ctx),
        "should return the same Arc"
    );
}

#[test]
fn test_torrent_attribute_on_download_context() {
    use crate::download::download_context::{BtFileMode, ContextAttributeType, TorrentAttribute};

    let group = RequestGroup::new(
        GroupId::new(3),
        vec!["bt://test".to_string()],
        DownloadOptions::default(),
    );

    let info_hash = "0123456789abcdef0123456789abcdef01234567";
    let mut ctx = DownloadContext::new(1024, 4096, "/tmp/file.bin".into());
    let ta = TorrentAttribute {
        name: "test-torrent".to_string(),
        mode: BtFileMode::Single,
        announce_list: vec![vec!["http://tracker.example.com/announce".to_string()]],
        nodes: Vec::new(),
        info_hash: info_hash.to_string(),
        metadata: Vec::new(),
        metadata_size: 0,
        private_torrent: false,
        creation_date: 0,
        comment: String::new(),
        created_by: String::new(),
        url_list: Vec::new(),
    };
    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
    group.set_download_context(Arc::new(ctx));

    let ctx_ref = group.get_download_context().unwrap();
    let hash = ctx_ref.get_bt_info_hash_hex();
    assert_eq!(hash, Some(info_hash.to_string()));
}
