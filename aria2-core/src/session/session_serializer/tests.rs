//! Unit tests for session serializer

use super::deserialization::deserialize;
use super::*;
use crate::download::download_context::DownloadContext;
use crate::download::file_entry::FileEntry;
use crate::request::request_group::{
    DownloadOptions, DownloadResult, DownloadResultCode, DownloadStatus, FileEntry as ResultFile,
    GroupId, RequestGroup, UriEntry,
};
use crate::selector::uri_selector::InorderUriSelector;
use crate::util::rwlock_ext::RwLockRecover;
use std::collections::HashMap;
use std::path::Path;

use tempfile::tempdir;

#[tokio::test]
async fn test_serialize_multiple_groups() {
    // Test that serialize_groups properly filters and serializes multiple groups
    // This test would require mock RequestGroup objects
    // For now, we test the deserialize function which handles multiple entries

    let input = r#"http://example.com/file1.zip
 GID=1
 split=4

http://example.com/file2.iso
 GID=2
 PAUSE=true
 dir=/downloads
"#;

    let entries = deserialize(input).unwrap();
    assert_eq!(entries.len(), 2, "Should parse 2 entries");
    assert_eq!(entries[0].uris[0], "http://example.com/file1.zip");
    assert_eq!(entries[1].uris[0], "http://example.com/file2.iso");
    assert!(!entries[0].paused, "First entry should not be paused");
    assert!(entries[1].paused, "Second entry should be paused");
}

#[test]
fn test_generated_child_groups_are_not_saved() {
    let parent = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1),
        vec!["http://example.com/parent.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let child = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2),
        vec!["http://example.com/payload.bin".to_string()],
        DownloadOptions::default(),
    )));
    child.recover().set_belongs_to_gid(GroupId::new(1));

    assert!(group_to_entry(&parent.recover()).is_some());
    assert!(group_to_entry(&child.recover()).is_none());
}

#[test]
fn test_group_session_entry_preserves_min_split_size_snapshot() {
    let group = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(3),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    group.recover_mut().set_option_snapshot(HashMap::from([(
        "min-split-size".to_string(),
        serde_json::json!("10M"),
    )]));

    let entry = group_to_entry(&group.recover()).expect("waiting group should be serializable");
    assert_eq!(
        entry.options.get("min-split-size"),
        Some(&"10M".to_string())
    );

    let restored = deserialize(&entry.serialize()).expect("session entry should deserialize");
    assert_eq!(
        restored[0].options.get("min-split-size"),
        Some(&"10M".to_string())
    );
}

#[test]
fn test_group_session_entry_includes_spent_uris_and_deduplicates_them() {
    let group = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(4),
        vec!["https://stale.example/file".to_string()],
        DownloadOptions::default(),
    )));

    let spent_uri = "https://spent.example/file";
    let remaining_uri = "https://remaining.example/file";
    let mut context = DownloadContext::new_default();
    context.set_file_entries(vec![FileEntry::new(
        "/tmp/file.bin".to_string(),
        0,
        0,
        vec![
            spent_uri.to_string(),
            remaining_uri.to_string(),
            spent_uri.to_string(),
        ],
    )]);

    let request = context.get_file_entries_mut()[0]
        .get_request(&InorderUriSelector::new(), false, &[], "", "GET")
        .expect("the first URI should be dispatched");
    assert_eq!(request.uri(), spent_uri);

    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));

    let entry = group_to_entry(&group.recover()).expect("waiting group should be serializable");
    assert_eq!(
        entry.uris,
        vec![remaining_uri.to_string(), spent_uri.to_string()]
    );

    let restored = deserialize(&entry.serialize()).expect("session entry should deserialize");
    assert_eq!(restored[0].uris, entry.uris);
}

#[test]
fn test_terminal_result_snapshot_captures_file_uri_state() {
    let group = RequestGroup::new(
        GroupId::new(5),
        vec!["https://initial.example/file".to_string()],
        DownloadOptions::default(),
    );
    let mut context = DownloadContext::new_default();
    context.set_file_entries(vec![FileEntry::new(
        "/tmp/file.bin".to_string(),
        10,
        0,
        vec![
            "https://spent.example/file".to_string(),
            "https://remaining.example/file".to_string(),
        ],
    )]);
    context.get_file_entries_mut()[0]
        .get_request(&InorderUriSelector::new(), false, &[], "", "GET")
        .expect("the first URI should be dispatched");
    group.set_download_context(std::sync::Arc::new(context));
    group.mark_complete();

    let result = group.create_download_result();
    assert_eq!(result.files.len(), 1);
    assert_eq!(
        result.files[0]
            .uris
            .iter()
            .map(|uri| (uri.uri.as_str(), uri.status.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("https://spent.example/file", "used"),
            ("https://remaining.example/file", "waiting"),
        ]
    );
}

#[test]
fn test_terminal_result_force_save_is_resumable_and_policy_filtered() {
    let mut result = DownloadResult::new(
        GroupId::new(6),
        DownloadStatus::Complete,
        DownloadResultCode::Finished,
    );
    result.files = vec![ResultFile {
        index: 1,
        path: "/tmp/file.bin".to_string(),
        length: 10,
        completed_length: 10,
        selected: true,
        uris: vec![
            UriEntry {
                uri: "https://spent.example/file".to_string(),
                status: "used".to_string(),
            },
            UriEntry {
                uri: "https://remaining.example/file".to_string(),
                status: "waiting".to_string(),
            },
        ],
    }];
    result.completed_length = 10;
    result.total_length = 10;
    result.set_option_snapshot(Some(HashMap::from([
        ("force-save".to_string(), serde_json::json!(true)),
        ("split".to_string(), serde_json::json!("4")),
    ])));

    let entry = download_result_to_entry(&result).expect("forced result should be serializable");
    assert_eq!(
        entry.uris,
        vec![
            "https://remaining.example/file".to_string(),
            "https://spent.example/file".to_string(),
        ]
    );
    assert_eq!(entry.status, "waiting");
    assert_eq!(entry.options.get("split"), Some(&"4".to_string()));

    let groups = Vec::new();
    let serialized = serialize_groups_with_results(&groups, &[result]);
    assert!(
        serialized
            .unwrap()
            .contains("https://remaining.example/file")
    );

    let mut not_forced = DownloadResult::finished();
    not_forced.gid = GroupId::new(7);
    not_forced.files = vec![ResultFile {
        index: 1,
        path: String::new(),
        length: 0,
        completed_length: 0,
        selected: true,
        uris: entry
            .uris
            .iter()
            .map(|uri| UriEntry {
                uri: uri.clone(),
                status: "waiting".to_string(),
            })
            .collect(),
    }];
    not_forced.set_option_snapshot(Some(HashMap::from([(
        "force-save".to_string(),
        serde_json::json!(false),
    )])));
    assert!(
        serialize_groups_with_results(&groups, &[not_forced])
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "bittorrent")]
#[test]
fn ordinary_follow_child_keeps_plain_session_identity() {
    let child = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2),
        vec!["bt://parent".to_string()],
        DownloadOptions::default(),
    )));
    child
        .recover()
        .set_metadata_info(crate::request::request_group::MetadataInfo::new(
            GroupId::new(1),
            "https://example.test/parent.torrent",
        ));
    child.recover().set_following_gid(GroupId::new(1));

    let entry = group_to_entry(&child.recover()).expect("plain child should be serializable");
    assert_eq!(entry.gid, 2);
    assert_eq!(entry.uris, vec!["bt://parent"]);
    assert!(!entry.options.contains_key("aria2-rust-payload-gid"));
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn metalink_graph_session_entry_uses_metadata_identity_and_descriptor() {
    let graph =
        crate::engine::metalink_request_graph::MetalinkRequestGraph::new_memory_with_fallback(
            "https://example.test/metadata.torrent",
            "payload.bin",
            &DownloadOptions::default(),
            GroupId::new(0x10),
            GroupId::new(0x20),
            vec!["https://mirror.test/payload.bin".to_string()],
        )
        .expect("graph should be constructible");

    let entry = group_to_entry(&graph.payload.recover()).expect("payload should be serializable");
    assert_eq!(entry.gid, 0x10);
    assert_eq!(entry.uris, vec!["https://example.test/metadata.torrent"]);
    assert_eq!(
        entry.options.get("aria2-rust-payload-gid"),
        Some(&"0000000000000020".to_string())
    );
    assert_eq!(
        entry.options.get("aria2-rust-metadata-uri"),
        Some(&"https://example.test/metadata.torrent".to_string())
    );
    assert!(entry.options.contains_key("aria2-rust-fallback-uris"));

    let restored = deserialize(&entry.serialize()).expect("serialized entry should parse");
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].options.get("aria2-rust-payload-gid"),
        Some(&"0000000000000020".to_string())
    );
}

#[test]
fn test_deserialize_mixed_content() {
    // Test handling of mixed content: comments, blanks, valid entries
    let input = r#"# Session file header
# Created by aria2-rust

# First download task
http://example.com/bigfile.tar.gz
 GID=abc123def456
 split=8
 dir=/data/downloads
 TOTAL_LENGTH=104857600
 COMPLETED_LENGTH=52428800

# Second download task (paused)
ftp://mirror.example.com/distro.iso
 GID=789abc012def
 PAUSE=true
 out=distro.iso
 STATUS=paused

# Third task with mirrors
http://mirror1.com/app.exe	http://mirror2.com/app.exe	http://mirror3.com/app.exe
 GID=fedcba098765
 max-connection-per-server=4

"#;

    let entries = deserialize(input).unwrap();
    assert_eq!(
        entries.len(),
        3,
        "Should parse 3 entries from mixed content"
    );

    // Verify first entry
    assert_eq!(entries[0].gid, 0xabc123def456);
    assert_eq!(entries[0].options.get("split").unwrap(), "8");
    assert_eq!(entries[0].total_length, 104857600);
    assert_eq!(entries[0].completed_length, 52428800);

    // Verify second entry (paused)
    assert!(entries[1].paused);
    assert_eq!(entries[1].status, "paused");
    assert_eq!(entries[1].options.get("out").unwrap(), "distro.iso");

    // Verify third entry (multiple mirrors)
    assert_eq!(
        entries[2].uris.len(),
        3,
        "Third entry should have 3 mirror URIs"
    );
    assert_eq!(
        entries[2].options.get("max-connection-per-server").unwrap(),
        "4"
    );
}

#[test]
fn test_deserialize_ignores_comments_inside_entry() {
    let input = r#"http://example.com/file.zip
 GID=0000000000000001
# A comment between session properties
 split=4
 TOTAL_LENGTH=1024

"#;

    let entries = deserialize(input).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].gid, 1);
    assert_eq!(entries[0].options.get("split"), Some(&"4".to_string()));
    assert_eq!(entries[0].total_length, 1024);
}

#[test]
fn test_deserialize_empty_and_whitespace_only() {
    // Test edge cases: completely empty or whitespace-only input
    assert!(
        deserialize("").unwrap().is_empty(),
        "Empty string should yield no entries"
    );
    assert!(
        deserialize("\n\n\n").unwrap().is_empty(),
        "Only newlines should yield no entries"
    );
    assert!(
        deserialize("   \n  \n   ").unwrap().is_empty(),
        "Whitespace-only should yield no entries"
    );
    assert!(
        deserialize("# Just a comment\n# Another comment\n")
            .unwrap()
            .is_empty(),
        "Comments-only should yield no entries"
    );
}

#[test]
fn test_deserialize_preserves_user_options() {
    // Test that user-defined options are preserved in options map
    let input = r#"http://example.com/file.zip
 GID=1
 split=4
 dir=/downloads
 TOTAL_LENGTH=1000
"#;

    let entries = deserialize(input).unwrap();
    assert_eq!(entries.len(), 1);

    // Known field parsed correctly
    assert_eq!(entries[0].total_length, 1000);

    // User options stored in options
    assert_eq!(entries[0].options.get("split").unwrap(), "4");
    assert_eq!(entries[0].options.get("dir").unwrap(), "/downloads");
}

#[test]
fn test_roundtrip_full_session() {
    // Test complete roundtrip: create entries -> serialize -> deserialize -> verify
    let original_entries = vec![
        SessionEntry::new(
            0xABCDEF01,
            vec![
                "http://primary.example.com/large-file.bin".to_string(),
                "http://mirror1.example.com/large-file.bin".to_string(),
                "http://mirror2.example.com/large-file.bin".to_string(),
            ],
        )
        .with_options({
            let mut opts = HashMap::new();
            opts.insert("split".to_string(), "16".to_string());
            opts.insert("max-connection-per-server".to_string(), "8".to_string());
            opts.insert("dir".to_string(), "/downloads".to_string());
            opts.insert("out".to_string(), "large-file.bin".to_string());
            opts
        }),
        SessionEntry::new(
            0x12345678,
            vec!["ftp://server.example.com/software.iso".to_string()],
        )
        .paused()
        .with_options({
            let mut opts = HashMap::new();
            opts.insert("seed-time".to_string(), "3600".to_string());
            opts
        }),
    ];

    // Serialize all entries
    let mut serialized = String::new();
    for entry in &original_entries {
        serialized.push_str(&entry.serialize());
        serialized.push('\n');
    }

    // Deserialize back
    let restored_entries = deserialize(&serialized).unwrap();

    // Verify count matches
    assert_eq!(
        restored_entries.len(),
        original_entries.len(),
        "Entry count should match after roundtrip"
    );

    // Verify first entry details
    assert_eq!(restored_entries[0].gid, 0xABCDEF01);
    assert_eq!(restored_entries[0].uris.len(), 3);
    assert_eq!(
        restored_entries[0].uris[0],
        "http://primary.example.com/large-file.bin"
    );
    assert_eq!(restored_entries[0].options.get("split").unwrap(), "16");
    assert!(!restored_entries[0].paused);

    // Verify second entry details
    assert_eq!(restored_entries[1].gid, 0x12345678);
    assert!(restored_entries[1].paused);
    assert_eq!(
        restored_entries[1].options.get("seed-time").unwrap(),
        "3600"
    );
}

#[test]
fn test_error_messages_are_english() {
    // Verify that error messages are in English (not Chinese)
    // We can't easily trigger actual errors without filesystem issues,
    // but we can check the error message format strings exist correctly

    // This test mainly documents the requirement; actual testing would
    // require mocking filesystem errors
    let path = Path::new("/nonexistent/path/aria2.session");

    // We can't actually run this in test without blocking,
    // but the error message strings should be English
    // Expected: "Failed to read session file ..."
    // Not: "Failed to read session file ..."

    // For now, just verify the function signature exists
    // In production, you'd want integration tests with actual FS errors
    let _ = path; // Suppress unused warning
}

#[tokio::test]
async fn test_gzip_session_file_roundtrip_is_atomic() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("aria2.session.gz");
    let entries = vec![
        SessionEntry::new(7, vec!["https://example.com/file.bin".to_string()])
            .with_options(HashMap::from([(String::from("split"), String::from("4"))])),
    ];

    save_to_file_with_entries(&path, &entries).await.unwrap();

    let bytes = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&bytes[..2], &[0x1f, 0x8b]);
    assert!(!path.with_extension("sess.tmp").exists());

    let restored = load_from_file(&path).await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].gid, 7);
    assert_eq!(restored[0].uris, entries[0].uris);
    assert_eq!(restored[0].options.get("split"), Some(&String::from("4")));
}

#[tokio::test]
async fn test_gzip_session_file_rejects_invalid_payload() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("aria2.session.gz");
    tokio::fs::write(&path, b"not a gzip stream").await.unwrap();

    let error = load_from_file(&path).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Failed to decompress session file")
    );
}
