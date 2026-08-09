//! Tests for the App module

use super::cli::CliArgs;
use super::*;
use aria2_core::config::OptionValue;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::util::rwlock_ext::RwLockRecover;
use clap::Parser;
use std::collections::HashMap;
use tempfile::TempDir;

#[tokio::test]
async fn test_cli_metalink_options_reach_download_options() {
    let cli = CliArgs::try_parse_from([
        "aria2",
        "--follow-metalink=mem",
        "--metalink-version=4.0",
        "--metalink-language=en",
        "--metalink-os=linux",
        "--metalink-location=us,jp",
        "--metalink-preferred-protocol=https",
        "--select-file=2",
        "https://example.test/metadata",
    ])
    .expect("Metalink CLI options should parse");

    let mut app = App::new();
    app.load_cli_args(cli)
        .await
        .expect("CLI options should load into ConfigManager");
    let options = app.download_options().await;

    assert_eq!(
        options.follow_metalink,
        Some(aria2_core::request::request_group::FollowMode::Memory)
    );
    assert_eq!(options.metalink_version.as_deref(), Some("4.0"));
    assert_eq!(options.metalink_language.as_deref(), Some("en"));
    assert_eq!(options.metalink_os.as_deref(), Some("linux"));
    assert_eq!(options.metalink_location.as_deref(), Some("us,jp"));
    assert_eq!(
        options.metalink_preferred_protocol.as_deref(),
        Some("https")
    );
    assert_eq!(options.select_file.as_deref(), Some("2"));
}

#[tokio::test]
async fn test_load_cli_args_rejects_invalid_split() {
    let cli = CliArgs::try_parse_from(["aria2", "--split=0"])
        .expect("clap should parse the integer before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("the registry must reject split=0");

    assert!(error.contains("--split"), "unexpected error: {error}");
}

#[tokio::test]
async fn test_load_cli_args_rejects_invalid_file_allocation() {
    let cli = CliArgs::try_parse_from(["aria2", "--file-allocation=invalid"])
        .expect("clap should parse the string before registry validation");
    let mut app = App::new();

    let error = app
        .load_cli_args(cli)
        .await
        .expect_err("the registry must reject an unknown allocation mode");

    assert!(
        error.contains("--file-allocation"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn test_no_conf_skips_explicit_config_file() {
    let temp_dir = TempDir::new().expect("temporary config directory");
    let config_path = temp_dir.path().join("aria2.conf");
    tokio::fs::write(&config_path, "split=8\n")
        .await
        .expect("write config file");

    let mut app = App::new();
    app.load_startup_config(true, config_path.to_str())
        .await
        .expect("--no-conf should not attempt to read the file");

    assert_eq!(app.get_opt_i64("split").await, Some(5));
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[tokio::test]
async fn test_standard_session_restores_metalink_graph() {
    use aria2_core::session::session_entry::SessionEntry;

    let temp_dir = TempDir::new().expect("temporary session directory");
    let session_file = temp_dir.path().join("aria2.session");
    let metadata_path = temp_dir.path().join("metadata.torrent");
    let mut options = HashMap::new();
    options.insert(
        "dir".to_string(),
        temp_dir.path().to_string_lossy().into_owned(),
    );
    options.insert(
        "aria2-rust-payload-gid".to_string(),
        "0000000000000020".to_string(),
    );
    options.insert(
        "aria2-rust-metadata-uri".to_string(),
        "https://example.test/metadata.torrent".to_string(),
    );
    options.insert(
        "aria2-rust-metadata-path".to_string(),
        metadata_path.to_string_lossy().into_owned(),
    );
    options.insert("aria2-rust-metadata-memory".to_string(), "true".to_string());
    options.insert(
        "aria2-rust-output-name".to_string(),
        "payload.bin".to_string(),
    );
    let entry = SessionEntry::new(
        0x10,
        vec!["https://example.test/metadata.torrent".to_string()],
    )
    .with_options(options);
    tokio::fs::write(&session_file, entry.serialize())
        .await
        .expect("write session");

    let app = App::new();
    {
        let mut config = app.config.write().await;
        config
            .set_global_option(
                "input-file",
                OptionValue::Str(session_file.to_string_lossy().into_owned()),
            )
            .await
            .expect("configure session input");
    }

    assert_eq!(app.restore_session().await.expect("restore session"), 2);
    let groups = app.request_man.read().await.list_groups();
    let metadata = groups
        .iter()
        .find(|group| {
            group.recover().gid() == aria2_core::request::request_group::GroupId::new(0x10)
        })
        .expect("metadata group should be restored");
    assert_eq!(
        metadata.recover().belongs_to_gid(),
        Some(aria2_core::request::request_group::GroupId::new(0x20))
    );
    let payload = groups
        .iter()
        .find(|group| {
            group.recover().gid() == aria2_core::request::request_group::GroupId::new(0x20)
        })
        .expect("payload group should be restored");
    assert_eq!(
        payload.recover().output_name().as_deref(),
        Some("payload.bin")
    );
    assert!(!payload.recover().is_dependency_resolved());
}

/// Test 1: Load entries from session file
///
/// Verify that restore_session() correctly loads and restores entries from a mock session file
#[tokio::test]
async fn test_input_file_loads_entries() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("test_session.txt");

    // Create a test session file with 3 entries
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/file1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=524288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=524288

http://example.com/file2.iso
 GID=2
 split=4
 dir=/downloads
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

ftp://server.com/bigfile.bin
 GID=3
 TOTAL_LENGTH=1073741824
 COMPLETED_LENGTH=536870912
 STATUS=paused
 ERROR_CODE=
 BITFIELD=fff00f
 NUM_PIECES=24
 PIECE_LENGTH=262144
 INFO_HASH=abc123def456
 RESUME_OFFSET=536870912
"#;

    // Write session file
    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    // Create App instance and configure input-file
    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    // Call restore method
    let result = app.restore_session().await;

    // Verify result
    assert!(result.is_ok(), "Restore should succeed");
    let count = result.unwrap();

    // Should restore 2 entries (skip file2 with completed_length=0 and total_length>0)
    // But according to our logic: completed_length=0 && total_length=0 is skipped
    // file2: completed_length=0, total_length=10485760 -> not skipped
    // So should restore 3 entries (none have complete status)
    assert_eq!(count, 3, "Should restore 3 non-completed entries");

    // Verify RequestGroupMan has corresponding groups
    let man = app.request_man.read().await;
    let group_count = man.count();
    assert_eq!(group_count, 3, "RequestGroupMan should have 3 groups");
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(1))
            .is_some()
    );
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(2))
            .is_some()
    );
    assert!(
        man.find_group(aria2_core::request::request_group::GroupId::new(3))
            .is_some()
    );
}

/// Test 2: Skip completed entries
///
/// Verify that entries with status "complete" are correctly skipped during restoration
#[tokio::test]
async fn test_skip_completed_entries() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("test_complete_session.txt");

    // Create session file with completed entries
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/complete1.zip
 GID=1
 TOTAL_LENGTH=1048576
 COMPLETED_LENGTH=1048576
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1048576

http://example.com/active2.zip
 GID=2
 TOTAL_LENGTH=2048576
 COMPLETED_LENGTH=1024288
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=1024288

http://example.com/complete3.bin
 GID=3
 TOTAL_LENGTH=512
 COMPLETED_LENGTH=512
 STATUS=complete
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=512

http://example.com/paused4.iso
 GID=4
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 STATUS=paused
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Restore should succeed");
    let count = result.unwrap();

    // Should only restore 2 entries (active and paused), skip 2 complete
    assert_eq!(count, 2, "Should only restore 2 non-completed entries");

    let man = app.request_man.read().await;
    let group_count = man.count();
    assert_eq!(group_count, 2, "RequestGroupMan should have 2 groups");
}

/// Test 3: Save session on shutdown
///
/// Verify that save_session_on_shutdown() correctly saves when save-session is configured
#[tokio::test]
async fn test_save_session_on_shutdown() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let save_file = temp_dir.path().join("shutdown_save.txt");

    let app = App::new();

    // Configure save-session option
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "save-session",
            OptionValue::Str(save_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set save-session");
        conf.set_global_option("save-session-interval", OptionValue::Str("60".to_string()))
            .await
            .expect("Failed to set save-session-interval");
    }

    // Add some download tasks to RequestGroupMan
    let opts = DownloadOptions {
        dir: Some("/downloads".to_string()),
        ..Default::default()
    };

    {
        let man = app.request_man.read().await;
        man.add_group(
            vec!["http://example.com/file1.zip".to_string()],
            opts.clone(),
        )
        .expect("Failed to add group 1");

        man.add_group(vec!["http://mirror.com/file2.iso".to_string()], opts)
            .expect("Failed to add group 2");
    }

    // Call shutdown save
    let result = app.save_session_on_shutdown().await;

    // Verify result
    assert!(result.is_ok(), "Save should succeed");
    let saved_count = result.expect("Should have a return value");
    assert!(
        saved_count.is_some(),
        "Should return Some when save-session is configured"
    );
    assert_eq!(saved_count.unwrap(), 2, "Should save 2 active tasks");

    // Verify file was created and contains correct URIs
    assert!(save_file.exists(), "Session file should exist after save");

    let content = tokio::fs::read_to_string(&save_file)
        .await
        .expect("Failed to read saved file");
    assert!(
        content.contains("http://example.com/file1.zip"),
        "File should contain the first URI"
    );
    assert!(
        content.contains("http://mirror.com/file2.iso"),
        "File should contain the second URI"
    );
}

/// Test 4: No save when save-session is not configured
///
/// Verify that save_session_on_shutdown() returns Ok(None) when save-session is not configured
#[tokio::test]
async fn test_no_save_when_not_configured() {
    let app = App::new();

    // Do not configure save-session

    let result = app.save_session_on_shutdown().await;

    assert!(result.is_ok(), "Should return Ok when not configured");
    assert!(
        result.unwrap().is_none(),
        "Should return None when save-session is not configured"
    );
}

/// Test 5: map_entry_to_download_options correctly maps options
#[test]
fn test_map_entry_to_download_options() {
    let mut options = HashMap::new();
    options.insert("split".to_string(), "8".to_string());
    options.insert("dir".to_string(), "/tmp/downloads".to_string());
    options.insert("out".to_string(), "output.bin".to_string());
    options.insert("max-download-limit".to_string(), "102400".to_string());
    options.insert("bt-force-encrypt".to_string(), "true".to_string());
    options.insert("enable-dht".to_string(), "false".to_string());

    let opts = App::map_entry_to_download_options(&options);

    assert_eq!(opts.split, Some(8), "split should map correctly");
    assert_eq!(
        opts.dir,
        Some("/tmp/downloads".to_string()),
        "dir should map correctly"
    );
    assert_eq!(
        opts.out,
        Some("output.bin".to_string()),
        "out should map correctly"
    );
    assert_eq!(
        opts.max_download_limit,
        Some(102400),
        "max-download-limit should map correctly"
    );
    assert!(
        opts.bt_force_encrypt,
        "bt-force-encrypt=true should map correctly"
    );
    assert!(!opts.enable_dht, "enable-dht=false should map correctly");
}

/// Test 6: Graceful handling of non-existent session file
#[tokio::test]
async fn test_restore_nonexistent_session_file() {
    let app = App::new();

    // Configure to point to non-existent file
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str("/nonexistent/path/session.txt".to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;

    // Should return Ok(0) when file doesn't exist, not error
    assert!(result.is_ok(), "Should return Ok when file does not exist");
    assert_eq!(
        result.unwrap(),
        0,
        "Should return 0 restored entries when file does not exist"
    );
}

/// Test 7: No restore when input-file is not configured
#[tokio::test]
async fn test_restore_without_input_file() {
    let app = App::new();

    // Do not configure input-file

    let result = app.restore_session().await;

    assert!(result.is_ok(), "Should return Ok when not configured");
    assert_eq!(
        result.unwrap(),
        0,
        "Should return 0 when input-file is not configured"
    );
}

/// Test 8: BT bitfield preserved on restore
#[tokio::test]
async fn test_bt_bitfield_preserved_on_restore() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("bt_session.txt");

    // Create session entry with BT bitfield
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"magnet:?xt=urn:btih:abc123def456
 GID=1
 TOTAL_LENGTH=104857600
 COMPLETED_LENGTH=52428800
 STATUS=active
 ERROR_CODE=
 BITFIELD=ffaabb
 NUM_PIECES=20
 PIECE_LENGTH=5242880
 INFO_HASH=abc123def456
 RESUME_OFFSET=52428800
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Restore should succeed");
    assert_eq!(result.unwrap(), 1, "Should restore 1 BT task");

    // Verify bitfield is preserved in RequestGroup
    let man = app.request_man.read().await;
    let groups = man.list_groups();
    assert_eq!(groups.len(), 1, "Should have 1 group");

    let group = groups[0].read().unwrap();
    let bitfield = group.bt_bitfield.read().unwrap();
    assert!(bitfield.is_some(), "BT bitfield should be preserved");
    assert_eq!(
        bitfield.as_ref().unwrap(),
        &vec![0xFF, 0xAA, 0xBB],
        "bitfield value should be correct"
    );
}

/// Test 9: Graceful handling of empty session file
#[tokio::test]
async fn test_restore_empty_session_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("empty_session.txt");

    // Create empty session file
    tokio::fs::write(&session_file, "")
        .await
        .expect("Failed to write empty file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Empty file should return Ok");
    assert_eq!(
        result.unwrap(),
        0,
        "Empty file should return 0 restored entries"
    );
}

/// Test 10: Skip entries with zero progress
#[tokio::test]
async fn test_skip_entries_with_zero_progress() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let session_file = temp_dir.path().join("zero_progress_session.txt");

    // Create session file where all entries have no progress
    // Per C++ aria2 behavior, 0/0 entries are still restored (they may
    // be newly added downloads that haven't started yet). Only "removed"
    // entries are skipped.
    // Note: Property lines must have leading space prefix (aria2 session format)
    let session_content = r#"http://example.com/new1.zip
 GID=1
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=

http://example.com/new2.iso
 GID=2
 TOTAL_LENGTH=0
 COMPLETED_LENGTH=0
 STATUS=waiting
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=
"#;

    tokio::fs::write(&session_file, session_content)
        .await
        .expect("Failed to write session file");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("Failed to set input-file");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "Should return Ok");
    // C++ aria2 restores ALL non-finished entries, including 0/0 progress
    // entries (they may be newly added downloads). Only "removed" entries
    // are skipped.
    assert_eq!(
        result.unwrap(),
        2,
        "C++ aria2 restores all non-finished entries including 0/0 progress"
    );

    let man = app.request_man.read().await;
    let group_count = man.count();
    assert_eq!(group_count, 2, "Should restore both 0/0 progress groups");
}
