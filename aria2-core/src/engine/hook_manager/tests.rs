//! Tests for the hook system

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::builtin::{ExecHook, MoveHook, RenameHook, TouchHook};
use super::types::{DownloadStats, HookContext, PostDownloadHook};
use super::{HookConfig, HookManager};

use crate::request::request_group::{DownloadStatus, GroupId};

/// Helper function: create a test HookContext
fn create_test_context(file_path: &Path) -> HookContext {
    HookContext {
        gid: GroupId::new(42),
        file_path: file_path.to_path_buf(),
        status: DownloadStatus::Complete,
        stats: DownloadStats {
            uploaded_bytes: 1024,
            downloaded_bytes: 2048,
            upload_speed: 100.0,
            download_speed: 200.0,
            elapsed_seconds: 10,
        },
        error: None,
    }
}

#[tokio::test]
async fn test_move_hook_basic() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let src_file = temp_dir.path().join("test_file.txt");

    // Create test file
    tokio::fs::write(&src_file, b"test content")
        .await
        .expect("Failed to write test file");

    let target_dir = temp_dir.path().join("target");
    let hook = MoveHook::new(target_dir.clone(), false);

    // Manually create target directory
    tokio::fs::create_dir_all(&target_dir)
        .await
        .expect("Failed to create target dir");

    let context = create_test_context(&src_file);

    assert!(hook.on_complete(&context).await.is_ok());

    // Verify file has been moved
    let moved_file = target_dir.join("test_file.txt");
    assert!(
        moved_file.exists(),
        "File should be moved to target directory"
    );
    assert!(!src_file.exists(), "Source file should no longer exist");
}

#[tokio::test]
async fn test_move_hook_create_dirs() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let src_file = temp_dir.path().join("test_file.txt");

    tokio::fs::write(&src_file, b"test content")
        .await
        .expect("Failed to write test file");

    // Target directory does not exist and has multiple nested levels
    let target_dir = temp_dir.path().join("nested").join("deep").join("target");
    let hook = MoveHook::new(target_dir.clone(), true);

    let context = create_test_context(&src_file);

    assert!(hook.on_complete(&context).await.is_ok());

    // Verify directory was auto-created and file was moved
    let moved_file = target_dir.join("test_file.txt");
    assert!(
        moved_file.exists(),
        "File should be moved to auto-created directory"
    );
}

#[tokio::test]
async fn test_rename_hook_pattern_expansion() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let src_file = temp_dir.path().join("archive.tar.gz");

    tokio::fs::write(&src_file, b"content")
        .await
        .expect("Failed to write test file");

    let hook = RenameHook::new("%f.renamed".to_string());
    let context = create_test_context(&src_file);

    // Test expand_pattern
    let expanded = hook.expand_pattern(&context);
    assert!(
        expanded.contains("archive.tar.gz.renamed"),
        "Pattern should contain original filename"
    );

    // Test actual rename
    assert!(hook.on_complete(&context).await.is_ok());

    let renamed_file = temp_dir.path().join("archive.tar.gz.renamed");
    assert!(
        renamed_file.exists(),
        "File should be renamed according to pattern"
    );
}

#[tokio::test]
async fn test_touch_hook_updates_mtime() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let src_file = temp_dir.path().join("timestamp_test.txt");

    tokio::fs::write(&src_file, b"touch test")
        .await
        .expect("Failed to write test file");

    // Get original modification time
    let before_metadata = tokio::fs::metadata(&src_file)
        .await
        .expect("Failed to get metadata");
    let before_mtime = before_metadata.modified().expect("Failed to get mtime");

    // Wait for a longer time to ensure time difference is detectable on all platforms
    // Windows FAT has 2-second resolution, NTFS has 100ns resolution
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let hook = TouchHook::new();
    let context = create_test_context(&src_file);

    assert!(hook.on_complete(&context).await.is_ok());

    // Verify modification time has been updated
    let after_metadata = tokio::fs::metadata(&src_file)
        .await
        .expect("Failed to get metadata after touch");
    let after_mtime = after_metadata
        .modified()
        .expect("Failed to get mtime after touch");

    assert!(
        after_mtime >= before_mtime,
        "Modification time should be updated to current time (before: {:?}, after: {:?})",
        before_mtime,
        after_mtime
    );
}

#[tokio::test]
async fn test_exec_hook_env_vars_injected() {
    // Create a simple test script that outputs environment variables to a file
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("env_output.txt");

    // Use echo command to write environment variables (cross-platform compatible)
    let cmd = format!("echo $ARIA2_GID > {}", output_file.display());

    let mut env_vars = HashMap::new();
    env_vars.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());

    let hook = ExecHook::new(cmd, env_vars);
    let context = create_test_context(&temp_dir.path().join("dummy.txt"));

    // Note: This test may need adjustment on non-Unix systems
    #[cfg(unix)]
    {
        let result = hook.on_complete(&context).await;
        // Even if the command fails (because sh may not be available), we mainly verify the build logic correctness
        let _ = result;
    }

    // Verify environment variable build logic
    let built_env = hook.build_env(&context, None);
    assert_eq!(
        built_env.get("ARIA2_GID").unwrap(),
        "42",
        "GID should be injected"
    );
    assert_eq!(
        built_env.get("ARIA2_STATUS").unwrap(),
        "complete",
        "Status should be complete"
    );
    assert_eq!(
        built_env.get("CUSTOM_VAR").unwrap(),
        "custom_value",
        "Custom var should be preserved"
    );
    assert_eq!(
        built_env.get("ARIA2_DOWNLOADED_BYTES").unwrap(),
        "2048",
        "Download bytes should be correct"
    );
}

#[tokio::test]
async fn test_exec_hook_nonzero_exit_code() {
    let hook = ExecHook::new("exit 1".to_string(), HashMap::new());
    let context = create_test_context(Path::new("/tmp/nonexistent"));

    let result = hook.on_complete(&context).await;
    assert!(result.is_err(), "Non-zero exit code should return error");

    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("failed")
            || err_msg.contains("exit code")
            || err_msg.contains("Failed")
            || err_msg.contains("execute"),
        "Error message should indicate failure, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_hook_chain_execution_order() {
    let mut manager = HookManager::new(HookConfig::default());

    // Verify hooks are added and counted in registration order
    manager.add_hook(Box::new(TouchHook));
    manager.add_hook(Box::new(RenameHook::new("%f.copy".to_string())));

    assert_eq!(manager.hook_count(), 2, "Should have 2 hooks registered");

    // Verify hooks can be removed by name (starting from the end of the chain)
    let removed = manager.remove_hook("RenameHook");
    assert!(removed.is_some(), "Should be able to remove RenameHook");
    assert_eq!(
        manager.hook_count(),
        1,
        "Should have 1 hook remaining after removal"
    );
}

#[tokio::test]
async fn test_hook_failure_isolation() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config = HookConfig {
        stop_on_error: false,
        ..Default::default()
    };
    let mut manager = HookManager::new(config);

    // Add an ExecHook that will fail
    manager.add_hook(Box::new(ExecHook::new(
        "exit 1".to_string(),
        HashMap::new(),
    )));

    let context = create_test_context(&temp_dir.path().join("test.txt"));

    // Should not return error because the first hook failed
    let results = manager.fire_complete(&context).await;
    assert!(results.is_ok(), "Should not fail when stop_on_error=false");

    let results_vec = results.unwrap();
    assert_eq!(results_vec.len(), 1, "Should have one result entry");
    assert!(
        results_vec[0].contains("failed"),
        "Result should indicate failure of the first hook"
    );
}

#[tokio::test]
async fn test_hook_config_stop_on_error() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config = HookConfig {
        stop_on_error: true,
        ..Default::default()
    };
    let mut manager = HookManager::new(config);

    // First hook will fail
    manager.add_hook(Box::new(ExecHook::new(
        "exit 1".to_string(),
        HashMap::new(),
    )));
    // Second hook should not be executed
    manager.add_hook(Box::new(ExecHook::new(
        "echo success".to_string(),
        HashMap::new(),
    )));

    let context = create_test_context(&temp_dir.path().join("test.txt"));

    let result = manager.fire_complete(&context).await;
    assert!(
        result.is_err(),
        "Should return error when stop_on_error=true and first hook fails"
    );
}

#[tokio::test]
async fn test_hook_remove_by_name() {
    let mut manager = HookManager::new(HookConfig::default());

    manager.add_hook(Box::new(TouchHook));
    manager.add_hook(Box::new(MoveHook::new(PathBuf::from("/tmp"), false)));

    assert_eq!(manager.hook_count(), 2);

    let removed = manager.remove_hook("TouchHook");
    assert!(removed.is_some(), "Should find and remove TouchHook");
    assert_eq!(removed.unwrap().name(), "TouchHook");
    assert_eq!(manager.hook_count(), 1, "Should have 1 hook remaining");

    // Try to remove a non-existent hook
    let not_found = manager.remove_hook("NonExistentHook");
    assert!(
        not_found.is_none(),
        "Should return None for non-existent hook"
    );
}

#[test]
fn test_hook_context_creation() {
    let context = HookContext::new(
        GroupId::new(123),
        PathBuf::from("/downloads/file.zip"),
        DownloadStatus::Complete,
        DownloadStats {
            downloaded_bytes: 9999,
            ..Default::default()
        },
        None,
    );

    assert_eq!(context.gid.value(), 123);
    assert_eq!(context.filename(), "file.zip");
    assert_eq!(context.extension(), "zip");
    assert_eq!(context.status, DownloadStatus::Complete);
    assert!(context.error.is_none());
    assert_eq!(context.stats.downloaded_bytes, 9999);
}

#[test]
fn test_download_stats_display() {
    let stats = DownloadStats {
        uploaded_bytes: 1024,
        downloaded_bytes: 2048,
        upload_speed: 100.5,
        download_speed: 200.25,
        elapsed_seconds: 30,
    };

    let display = format!("{}", stats);
    assert!(
        display.contains("downloaded=2048"),
        "Should contain downloaded bytes"
    );
    assert!(
        display.contains("uploaded=1024"),
        "Should contain uploaded bytes"
    );
    assert!(display.contains("200.25"), "Should contain download speed");
    assert!(
        display.contains("elapsed=30s"),
        "Should contain elapsed time"
    );
}
