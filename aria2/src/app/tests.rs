//! Tests for the App module

use super::*;
use aria2_core::config::OptionValue;
use aria2_core::request::request_group::DownloadOptions;
use std::collections::HashMap;
use tempfile::TempDir;

/// Test 1: Load entries from session file
///
/// Verify that restore_session() correctly loads and restores entries from a mock session file
#[tokio::test]
async fn test_input_file_loads_entries() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let session_file = temp_dir.path().join("test_session.txt");

    // Create a test session file with 3 entries
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
        .expect("写入会话文件失败");

    // Create App instance and configure input-file
    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("设置 input-file 失败");
    }

    // Call restore method
    let result = app.restore_session().await;

    // Verify result
    assert!(result.is_ok(), "恢复应成功");
    let count = result.unwrap();

    // Should restore 2 entries (skip file2 with completed_length=0 and total_length>0)
    // But according to our logic: completed_length=0 && total_length=0 is skipped
    // file2: completed_length=0, total_length=10485760 -> not skipped
    // So should restore 3 entries (none have complete status)
    assert_eq!(count, 3, "应恢复 3 个非完成状态的条目");

    // Verify RequestGroupMan has corresponding groups
    let man = app.request_man.read().await;
    let group_count = man.count().await;
    assert_eq!(group_count, 3, "RequestGroupMan 中应有 3 个组");
}

/// Test 2: Skip completed entries
///
/// Verify that entries with status "complete" are correctly skipped during restoration
#[tokio::test]
async fn test_skip_completed_entries() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let session_file = temp_dir.path().join("test_complete_session.txt");

    // Create session file with completed entries
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
        .expect("写入会话文件失败");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("设置 input-file 失败");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "恢复应成功");
    let count = result.unwrap();

    // Should only restore 2 entries (active and paused), skip 2 complete
    assert_eq!(count, 2, "应只恢复 2 个非完成状态的条目");

    let man = app.request_man.read().await;
    let group_count = man.count().await;
    assert_eq!(group_count, 2, "RequestGroupMan 中应有 2 个组");
}

/// Test 3: Save session on shutdown
///
/// Verify that save_session_on_shutdown() correctly saves when save-session is configured
#[tokio::test]
async fn test_save_session_on_shutdown() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
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
        .expect("设置 save-session 失败");
        conf.set_global_option("save-session-interval", OptionValue::Str("60".to_string()))
            .await
            .expect("设置 save-session-interval 失败");
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
        .await
        .expect("添加组 1 失败");

        man.add_group(vec!["http://mirror.com/file2.iso".to_string()], opts)
            .await
            .expect("添加组 2 失败");
    }

    // Call shutdown save
    let result = app.save_session_on_shutdown().await;

    // Verify result
    assert!(result.is_ok(), "保存应成功");
    let saved_count = result.expect("应有返回值");
    assert!(saved_count.is_some(), "配置了 save-session 时应返回 Some");
    assert_eq!(saved_count.unwrap(), 2, "应保存 2 个活动任务");

    // Verify file was created and contains correct URIs
    assert!(save_file.exists(), "保存后会话文件应存在");

    let content = tokio::fs::read_to_string(&save_file)
        .await
        .expect("读取保存的文件失败");
    assert!(
        content.contains("http://example.com/file1.zip"),
        "文件应包含第一个 URI"
    );
    assert!(
        content.contains("http://mirror.com/file2.iso"),
        "文件应包含第二个 URI"
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

    assert!(result.is_ok(), "未配置时应返回 Ok");
    assert!(
        result.unwrap().is_none(),
        "未配置 save-session 时应返回 None"
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

    assert_eq!(opts.split, Some(8), "split 应正确映射");
    assert_eq!(
        opts.dir,
        Some("/tmp/downloads".to_string()),
        "dir 应正确映射"
    );
    assert_eq!(opts.out, Some("output.bin".to_string()), "out 应正确映射");
    assert_eq!(
        opts.max_download_limit,
        Some(102400),
        "max-download-limit 应正确映射"
    );
    assert!(opts.bt_force_encrypt, "bt-force-encrypt=true 应正确映射");
    assert!(!opts.enable_dht, "enable-dht=false 应正确映射");
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
        .expect("设置 input-file 失败");
    }

    let result = app.restore_session().await;

    // Should return Ok(0) when file doesn't exist, not error
    assert!(result.is_ok(), "文件不存在时应返回 Ok");
    assert_eq!(result.unwrap(), 0, "文件不存在时应返回 0 个恢复条目");
}

/// Test 7: No restore when input-file is not configured
#[tokio::test]
async fn test_restore_without_input_file() {
    let app = App::new();

    // Do not configure input-file

    let result = app.restore_session().await;

    assert!(result.is_ok(), "未配置时应返回 Ok");
    assert_eq!(result.unwrap(), 0, "未配置 input-file 时应返回 0");
}

/// Test 8: BT bitfield preserved on restore
#[tokio::test]
async fn test_bt_bitfield_preserved_on_restore() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let session_file = temp_dir.path().join("bt_session.txt");

    // Create session entry with BT bitfield
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
        .expect("写入会话文件失败");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("设置 input-file 失败");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "恢复应成功");
    assert_eq!(result.unwrap(), 1, "应恢复 1 个 BT 任务");

    // Verify bitfield is preserved in RequestGroup
    let man = app.request_man.read().await;
    let groups = man.list_groups().await;
    assert_eq!(groups.len(), 1, "应有 1 个组");

    let group = groups[0].read().await;
    let bitfield = group.bt_bitfield.read().await;
    assert!(bitfield.is_some(), "BT bitfield 应被保留");
    assert_eq!(
        bitfield.as_ref().unwrap(),
        &vec![0xFF, 0xAA, 0xBB],
        "bitfield 值应正确"
    );
}

/// Test 9: Graceful handling of empty session file
#[tokio::test]
async fn test_restore_empty_session_file() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let session_file = temp_dir.path().join("empty_session.txt");

    // Create empty session file
    tokio::fs::write(&session_file, "")
        .await
        .expect("写入空文件失败");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("设置 input-file 失败");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "空文件应返回 Ok");
    assert_eq!(result.unwrap(), 0, "空文件应返回 0 个恢复条目");
}

/// Test 10: Skip entries with zero progress
#[tokio::test]
async fn test_skip_entries_with_zero_progress() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let session_file = temp_dir.path().join("zero_progress_session.txt");

    // Create session file where all entries have no progress
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
        .expect("写入会话文件失败");

    let app = App::new();
    {
        let mut conf = app.config.write().await;
        conf.set_global_option(
            "input-file",
            OptionValue::Str(session_file.to_string_lossy().to_string()),
        )
        .await
        .expect("设置 input-file 失败");
    }

    let result = app.restore_session().await;
    assert!(result.is_ok(), "应返回 Ok");
    assert_eq!(result.unwrap(), 0, "无进度的条目应全部被跳过");

    let man = app.request_man.read().await;
    let group_count = man.count().await;
    assert_eq!(group_count, 0, "不应添加任何组");
}
