use std::path::PathBuf;
use std::sync::Arc;

use super::session_serializer::{self, SessionEntry};
use crate::request::request_group::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

/// Active session manager responsible for session loading and explicit saving.
pub struct ActiveSessionManager {
    /// Session file path
    pub session_path: PathBuf,
}

impl ActiveSessionManager {
    /// Create a new active session manager
    ///
    /// # Arguments
    /// - `session_path`: Path where the session file is saved
    ///
    /// # Example
    /// ```ignore
    /// let manager = ActiveSessionManager::new(PathBuf::from("/tmp/session.txt"));
    /// ```
    pub fn new(session_path: PathBuf) -> Self {
        tracing::info!(
            "Creating ActiveSessionManager: path={}",
            session_path.display()
        );

        ActiveSessionManager { session_path }
    }

    /// Load session data from file
    ///
    /// If the file does not exist, returns an empty Vec (not treated as an error)
    ///
    /// # Returns
    /// - `Ok(Vec<SessionEntry>)`: Successfully loaded session entry list
    /// - `Err(String)`: Error message when loading fails
    pub async fn load_session(&self) -> Result<Vec<SessionEntry>, String> {
        if !self.session_path.exists() {
            tracing::debug!(
                "Session file not found, returning empty list: {}",
                self.session_path.display()
            );
            return Ok(vec![]);
        }

        match session_serializer::load_from_file(&self.session_path).await {
            Ok(entries) => {
                tracing::info!(
                    "Session file loaded successfully: {}, entries: {}",
                    self.session_path.display(),
                    entries.len()
                );
                Ok(entries)
            }
            Err(e) => {
                let err_msg = format!("Failed to load session file: {}", e);
                tracing::error!("{}", err_msg);
                Err(err_msg)
            }
        }
    }

    /// Save all download group states to the session file
    ///
    /// Uses atomic write strategy: writes to a temporary file (.sess.tmp) first,
    /// then renames to the target file. This ensures that if a crash occurs during
    /// writing, the original session file will not be corrupted.
    ///
    /// # Arguments
    /// - `groups`: List of download groups whose states need to be saved
    ///
    /// # Returns
    /// - `Ok(usize)`: Number of entries successfully saved
    /// - `Err(String)`: Error message when saving fails
    pub async fn save_session(
        &self,
        groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        // Serialize all groups into a SessionEntry list
        let mut entries = Vec::new();
        for group_lock in groups {
            let group = group_lock.recover();
            if let Some(entry) = session_serializer::group_to_entry(&group) {
                entries.push(entry);
            }
        }

        // Save to file using atomic write strategy
        match session_serializer::save_to_file_with_entries(&self.session_path, &entries).await {
            Ok(_) => {
                tracing::info!(
                    "Session file saved successfully: {}, entries: {}",
                    self.session_path.display(),
                    entries.len()
                );
                Ok(entries.len())
            }
            Err(e) => {
                let err_msg = format!("Failed to save session file: {}", e);
                tracing::error!("{}", err_msg);
                Err(err_msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use tempfile::TempDir;

    /// Test 1: Verify new() correctly creates the manager
    #[test]
    fn test_new_manager() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().join("session.txt");

        let manager = ActiveSessionManager::new(session_path.clone());

        assert_eq!(
            manager.session_path, session_path,
            "Path should be set correctly"
        );
    }

    /// Test 2: Return empty list when file does not exist
    #[tokio::test]
    async fn test_load_nonexistent_file_returns_empty() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let nonexistent_path = temp_dir.path().join("nonexistent_session.txt");

        let manager = ActiveSessionManager::new(nonexistent_path);
        let result = manager.load_session().await;

        assert!(result.is_ok(), "Non-existent file should not return error");
        let entries = result.unwrap();
        assert!(
            entries.is_empty(),
            "Non-existent file should return empty list"
        );
    }

    /// Test 3: Save and load roundtrip test
    #[tokio::test]
    async fn test_load_save_roundtrip() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().join("roundtrip_session.txt");

        let manager = ActiveSessionManager::new(session_path.clone());

        // Create test RequestGroups
        let gid1 = GroupId::new(0xd270c8a2);
        let options1 = DownloadOptions {
            dir: Some("/downloads".to_string()),
            split: Some(4),
            ..Default::default()
        };
        let group1 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid1,
            vec!["http://example.com/file1.zip".to_string()],
            options1,
        )));

        let gid2 = GroupId::new(0xabcdef01);
        let group2 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid2,
            vec![
                "http://mirror.com/file2.iso".to_string(),
                "ftp://backup.com/file2.iso".to_string(),
            ],
            DownloadOptions::default(),
        )));

        let groups = vec![group1, group2];

        // Save session
        let save_result = manager.save_session(&groups).await;
        assert!(save_result.is_ok(), "Save should succeed");
        let saved_count = save_result.unwrap();
        assert!(saved_count > 0, "Should save at least 1 entry");

        // Load session and verify
        let load_result = manager.load_session().await;
        assert!(load_result.is_ok(), "Load should succeed");
        let entries = load_result.unwrap();

        assert_eq!(
            entries.len(),
            saved_count,
            "Loaded entry count should match saved count"
        );

        // Verify data integrity
        assert!(
            entries
                .iter()
                .any(|e| e.uris.contains(&"http://example.com/file1.zip".to_string())),
            "Should contain the first URI"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.uris.contains(&"http://mirror.com/file2.iso".to_string())),
            "Should contain the second URI"
        );
    }

    /// Test 4: File should exist at specified path after saving
    #[tokio::test]
    async fn test_save_creates_file() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().join("file_creation_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone());

        // Verify file does not exist initially
        assert!(
            !session_path.exists(),
            "File should not exist before saving"
        );

        // Create test group
        let gid = GroupId::new(12345);
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec!["http://test.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));

        // Execute save
        let result = manager.save_session(&[group]).await;
        assert!(result.is_ok(), "Save should succeed");

        // Verify file was created
        assert!(
            session_path.exists(),
            "File should exist at specified path after saving"
        );

        // Verify file content is not empty
        let content = tokio::fs::read_to_string(&session_path)
            .await
            .expect("Failed to read file");
        assert!(!content.is_empty(), "File content should not be empty");
        assert!(
            content.contains("http://test.com/file.bin"),
            "File should contain the saved URI"
        );
    }

    /// Test 5: Multiple saves overwrite old file
    #[tokio::test]
    async fn test_multiple_saves_overwrite() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().join("overwrite_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone());

        // First save
        let gid1 = GroupId::new(1);
        let group1 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid1,
            vec!["http://first.com/a.txt".to_string()],
            DownloadOptions::default(),
        )));
        let result1 = manager.save_session(&[group1]).await;
        assert!(result1.is_ok());

        // Second save with different content
        let gid2 = GroupId::new(2);
        let group2 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid2,
            vec!["http://second.com/b.txt".to_string()],
            DownloadOptions::default(),
        )));
        let result2 = manager.save_session(&[group2]).await;
        assert!(result2.is_ok());

        // Load and verify only the second save's content is present
        let entries = manager.load_session().await.expect("Failed to load");
        assert_eq!(entries.len(), 1, "Should have only 1 entry (the latest)");
        assert!(
            entries[0]
                .uris
                .contains(&"http://second.com/b.txt".to_string()),
            "Should contain the latest saved URI"
        );
        assert!(
            !entries[0]
                .uris
                .contains(&"http://first.com/a.txt".to_string()),
            "Should not contain the old URI"
        );
    }

    /// Test 6: File does not exist or is empty after saving empty group list
    #[tokio::test]
    async fn test_save_empty_groups() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let session_path = temp_dir.path().join("empty_groups_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone());

        let stale_group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(123),
            vec!["http://stale.example/old.bin".to_string()],
            DownloadOptions::default(),
        )));
        manager
            .save_session(&[stale_group])
            .await
            .expect("seed stale session");

        // Save empty list to clear the stale entry.
        let empty_groups: Vec<Arc<std::sync::RwLock<RequestGroup>>> = vec![];
        let result = manager.save_session(&empty_groups).await;

        assert!(result.is_ok(), "Saving empty list should succeed");
        assert_eq!(result.unwrap(), 0, "Should return 0 entries");

        assert!(
            session_path.exists(),
            "Saving an empty group list must clear stale session state"
        );
        let content = tokio::fs::read_to_string(&session_path)
            .await
            .expect("Failed to read file");
        assert!(
            content.is_empty(),
            "Empty group list should produce an empty session file"
        );
    }
}
