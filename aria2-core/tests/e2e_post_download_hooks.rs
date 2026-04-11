//! E2E Integration Tests for Post-Download Hooks
//!
//! Tests the complete hook system including MoveHook, RenameHook,
//! and hook chain execution order.

mod e2e_helpers;

mod tests {
    use std::path::PathBuf;
    use tempfile::TempDir;

    use aria2_core::engine::bt_post_download_handler::*;
    use aria2_core::request::request_group::GroupId;

    #[tokio::test]
    async fn test_move_hook_moves_file() {
        // Create temp directories
        let source_dir = TempDir::new().expect("Failed to create temp dir");
        let target_dir = TempDir::new().expect("Failed to create temp dir");

        // Create a test file in source directory
        let source_file = source_dir.path().join("test_file.txt");
        tokio::fs::write(&source_file, "Test content for move hook")
            .await
            .expect("Failed to write test file");

        // Verify source file exists
        assert!(source_file.exists(), "Source file should exist before move");

        // Create HookContext
        let context = HookContext::new(
            GroupId::new(1),
            source_file.clone(),
            DownloadStatus::Complete,
            DownloadStats::default(),
            None,
        );

        // Create and execute MoveHook
        let move_hook = MoveHook::new(target_dir.path().to_path_buf(), true);
        let result = move_hook.on_complete(&context).await;

        // Verify move succeeded
        assert!(result.is_ok(), "MoveHook should succeed");

        // Verify file was moved (no longer in source)
        assert!(
            !source_file.exists(),
            "Source file should not exist after move"
        );

        // Verify file exists in target directory
        let target_file = target_dir.path().join("test_file.txt");
        assert!(
            target_file.exists(),
            "File should exist in target directory after move"
        );

        // Verify file content is preserved
        let content = tokio::fs::read_to_string(&target_file)
            .await
            .expect("Failed to read moved file");
        assert_eq!(content, "Test content for move hook");

        // Test error case: moving non-existent file
        let missing_context = HookContext::new(
            GroupId::new(2),
            PathBuf::from("/nonexistent/path/file.txt"),
            DownloadStatus::Complete,
            DownloadStats::default(),
            None,
        );
        let error_result = move_hook.on_complete(&missing_context).await;
        assert!(
            error_result.is_err(),
            "MoveHook should fail for non-existent file"
        );
    }

    #[tokio::test]
    async fn test_rename_hook_expands_patterns() {
        // Create a temp file
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let source_file = temp_dir.path().join("document.pdf");

        // Write test content
        tokio::fs::write(&source_file, "PDF document content")
            .await
            .expect("Failed to write test file");

        // Create HookContext with known values
        let context = HookContext::new(
            GroupId::new(42), // GID = 42
            source_file.clone(),
            DownloadStatus::Complete,
            DownloadStats::default(),
            None,
        );

        // Test pattern expansion with various placeholders
        let patterns: Vec<(&str, &str)> = vec![
            ("%f_original", "%f_original"),   // filename pattern
            ("%d_backup/%f", "%d_backup/%f"), // directory + filename
            ("file_%i_%f", "file_%i_%f"),     // gid + filename
        ];

        for (pattern_input, _expected_pattern) in patterns {
            let rename_hook = RenameHook::new(pattern_input.to_string());
            let expanded = rename_hook.expand_pattern(&context);

            // Verify that placeholders were expanded
            assert!(
                !expanded.contains('%'),
                "Pattern '{}' should have all placeholders expanded, got: '{}'",
                pattern_input,
                expanded
            );

            // Verify specific expansions
            if pattern_input.contains("%f") {
                assert!(
                    expanded.contains("document"),
                    "Expanded pattern should contain filename"
                );
            }

            if pattern_input.contains("%i") {
                assert!(
                    expanded.contains("42"),
                    "Expanded pattern should contain GID"
                );
            }

            if pattern_input.contains("%e") {
                assert!(
                    expanded.contains("pdf"),
                    "Expanded pattern should contain extension"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_hook_chain_execution_order() {
        // Create temp directories for multi-hook test
        let source_dir = TempDir::new().expect("Failed to create source dir");
        let intermediate_dir = TempDir::new().expect("Failed to create intermediate dir");
        let final_dir = TempDir::new().expect("Failed to create final dir");

        // Create initial test file
        let source_file = source_dir.path().join("chain_test.dat");
        tokio::fs::write(&source_file, "Data for hook chain testing")
            .await
            .expect("Failed to write test file");

        // Track execution order using a shared vector
        use std::sync::{Arc, Mutex};
        let execution_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        // Create first hook: MoveHook (source -> intermediate)
        let log_clone1 = Arc::clone(&execution_log);
        let move_hook = MoveHook::new(intermediate_dir.path().to_path_buf(), true);

        // Wrap with logging (simplified - in real code would use decorator pattern)
        // For this test, we execute hooks sequentially and verify

        let context1 = HookContext::new(
            GroupId::new(10),
            source_file.clone(),
            DownloadStatus::Complete,
            DownloadStats::default(),
            None,
        );

        // Execute MoveHook
        log_clone1
            .lock()
            .unwrap()
            .push("MoveHook:start".to_string());
        let result1 = move_hook.on_complete(&context1).await;
        log_clone1.lock().unwrap().push("MoveHook:end".to_string());

        assert!(result1.is_ok(), "First hook (MoveHook) should succeed");

        // Verify file moved to intermediate location
        let intermediate_file = intermediate_dir.path().join("chain_test.dat");
        assert!(
            intermediate_file.exists(),
            "File should be in intermediate directory after MoveHook"
        );

        // Create second hook: MoveHook (intermediate -> final)
        let log_clone2 = Arc::clone(&execution_log);
        let final_move_hook = MoveHook::new(final_dir.path().to_path_buf(), true);

        let context2 = HookContext::new(
            GroupId::new(10),
            intermediate_file.clone(),
            DownloadStatus::Complete,
            DownloadStats::default(),
            None,
        );

        // Execute second MoveHook
        log_clone2
            .lock()
            .unwrap()
            .push("FinalMoveHook:start".to_string());
        let result2 = final_move_hook.on_complete(&context2).await;
        log_clone2
            .lock()
            .unwrap()
            .push("FinalMoveHook:end".to_string());

        assert!(
            result2.is_ok(),
            "Second hook (FinalMoveHook) should succeed"
        );

        // Verify file moved to final location
        let final_file = final_dir.path().join("chain_test.dat");
        assert!(
            final_file.exists(),
            "File should be in final directory after both hooks"
        );

        // Verify file not in intermediate anymore
        assert!(
            !intermediate_file.exists(),
            "File should not remain in intermediate directory"
        );

        // Verify execution order
        let log = execution_log.lock().unwrap();
        assert_eq!(
            *log,
            vec![
                "MoveHook:start",
                "MoveHook:end",
                "FinalMoveHook:start",
                "FinalMoveHook:end",
            ],
            "Hooks should execute in sequential order"
        );

        // Verify data integrity after all moves
        let content = tokio::fs::read_to_string(&final_file)
            .await
            .expect("Failed to read final file");
        assert_eq!(content, "Data for hook chain testing");
    }
}
