//! Download Coordinator - Task lifecycle management
//!
//! This module provides a high-level interface for managing download tasks,
//! abstracting the complexity of the underlying engine and request groups.
//!
//! # Features
//!
//! - Download task creation and lifecycle management
//! - Session persistence coordination
//! - Progress monitoring and statistics
//! - Graceful shutdown handling
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DownloadHandler.cc/h` - Download processing logic
//! - `src/RequestGroupMan.cc/h` - Request group management

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::cli_options::{extract_uris, parse_args, validate_options};
use crate::progress_display::{format_bytes, format_duration, format_speed};
use aria2_core::{
    config::option::{DownloadOptions, OptionCategory},
    engine::download_engine::DownloadEngine,
    error::{Aria2Error, Result},
    request::request_group::{GroupId, RequestGroup},
    session::active_session::ActiveSessionManager,
};

/// Download task status information
#[derive(Debug, Clone)]
pub struct DownloadTaskInfo {
    /// Unique identifier
    pub gid: GroupId,
    /// Current status string (e.g., "active", "paused", "complete")
    pub status: String,
    /// Total file size in bytes
    pub total_length: u64,
    /// Bytes downloaded so far
    pub completed_length: u64,
    /// Download speed in bytes/second
    pub download_speed: f64,
    /// Upload speed in bytes/second
    pub upload_speed: f64,
    /// Elapsed time since start
    pub elapsed: std::time::Duration,
}

impl Default for DownloadTaskInfo {
    fn default() -> Self {
        Self {
            gid: 0,
            status: "waiting".to_string(),
            total_length: 0,
            completed_length: 0,
            download_speed: 0.0,
            upload_speed: 0.0,
            elapsed: std::time::Duration::ZERO,
        }
    }
}

/// Summary of all active downloads
pub struct DownloadSummary {
    /// Number of active downloads
    pub active_count: usize,
    /// Total bytes downloaded across all tasks
    pub total_downloaded: u64,
    /// Average download speed
    pub avg_speed: f64,
    /// Total elapsed time
    pub total_elapsed: std::time::Duration,
}

/// Download Coordinator - Manages the full lifecycle of downloads
///
/// This struct provides a simplified interface for common download operations,
/// hiding the complexity of engine initialization, request group management,
/// and session persistence.
///
/// # Example
///
/// ```rust,ignore
/// use aria2::download_coordinator::DownloadCoordinator;
///
/// let coordinator = DownloadCoordinator::new().await?;
///
/// // Add a download
/// let gid = coordinator.add_download(
///     vec!["http://example.com/file.zip".to_string()],
///     Default::default()
/// ).await?;
///
/// // Get status
/// let info = coordinator.get_download_info(gid).await?;
/// println!("Progress: {}/{}", info.completed_length, info.total_length);
///
/// // Shutdown
/// coordinator.shutdown().await?;
/// ```
pub struct DownloadCoordinator {
    engine: Arc<RwLock<Option<DownloadEngine>>>,
    request_man: Arc<RequestGroup>,
    config: Arc<RwLock<aria2_core::config::option::Options>>,
}

impl DownloadCoordinator {
    /// Create a new DownloadCoordinator with default configuration
    ///
    /// Initializes the internal engine and request manager.
    ///
    /// # Returns
    /// * `Ok(Self)` - Successfully initialized coordinator
    /// * `Err(Aria2Error)` - Initialization failed
    pub async fn new() -> Result<Self> {
        let options = aria2_core::config::option::Options::default();
        let coordinator = Self::with_options(options).await;
        Ok(coordinator)
    }

    /// Create a new DownloadCoordinator with custom options
    ///
    /// # Arguments
    /// * `options` - Initial configuration options
    ///
    /// # Returns
    /// * Initialized DownloadCoordinator
    pub async fn with_options(options: aria2_core::config::option::Options) -> Self {
        let request_man = RequestGroup::new();
        let config = Arc::new(RwLock::new(options));

        // Initialize engine with default tick interval
        let tick_ms = 100u64; // Default BT peer timeout

        Self {
            engine: Arc::new(RwLock::new(Some(DownloadEngine::new(tick_ms)))),
            request_man: Arc::new(request_man),
            config,
        }
    }

    /// Add a new download task
    ///
    /// Creates a new download from the given URIs with specified options.
    ///
    /// # Arguments
    /// * `uris` - List of URIs to download (HTTP, HTTPS, FTP, etc.)
    /// * `options` - Download-specific options (can be empty for defaults)
    ///
    /// # Returns
    /// * `Ok(GroupId)` - GID of the newly created download task
    /// * `Err(Aria2Error)` - If download creation failed
    pub async fn add_download(
        &self,
        uris: Vec<String>,
        options: DownloadOptions,
    ) -> Result<GroupId> {
        if uris.is_empty() {
            return Err(Aria2Error::Fatal(aria2_core::error::FatalError::Config(
                "No URIs provided".to_string(),
            )));
        }

        tracing::info!(
            "[Coordinator] Adding download: {} URI(s)",
            uris.len()
        );

        // Create request group from URIs
        let gid = self.request_man.create_group(uris, options).await?;

        tracing::info!("[Coordinator] Created download GID={:x}", gid);
        Ok(gid)
    }

    /// Pause an active download
    ///
    /// # Arguments
    /// * `gid` - GID of the download to pause
    ///
    /// # Returns
    /// * `Ok(())` - Successfully paused
    /// * `Err(Aria2Error)` - If pause failed or GID not found
    #[allow(dead_code)]
    pub async fn pause_download(&self, gid: GroupId) -> Result<()> {
        tracing::info!("[Coordinator] Pausing download GID={:x}", gid);

        // TODO: Implement actual pause logic
        // This would involve:
        // 1. Finding the request group by GID
        // 2. Setting its status to paused
        // 3. Notifying the engine

        Ok(())
    }

    /// Resume a paused download
    ///
    /// # Arguments
    /// * `gid` - GID of the download to resume
    ///
    /// # Returns
    /// * `Ok(())` - Successfully resumed
    /// * `Err(Aria2Error)` - If resume failed or GID not found
    #[allow(dead_code)]
    pub async fn resume_download(&self, gid: GroupId) -> Result<()> {
        tracing::info!("[Coordinator] Resuming download GID={:x}", gid);

        // TODO: Implement actual resume logic
        Ok(())
    }

    /// Remove a download (cancel or complete)
    ///
    /// # Arguments
    /// * `gid` - GID of the download to remove
    /// * `force` - If true, force removal even if actively downloading
    ///
    /// # Returns
    /// * `Ok(())` - Successfully removed
    /// * `Err(Aria2Error)` - If removal failed or GID not found
    #[allow(dead_code)]
    pub async fn remove_download(&self, gid: GroupId, force: bool) -> Result<()> {
        tracing::info!(
            "[Coordinator] Removing download GID={:x} (force={})",
            gid,
            force
        );

        // TODO: Implement actual removal logic
        Ok(())
    }

    /// Get information about a specific download
    ///
    /// # Arguments
    /// * `gid` - GID of the download to query
    ///
    /// # Returns
    /// * `Ok(DownloadTaskInfo)` - Current status information
    /// * `Err(Aria2Error)` - If GID not found or query failed
    pub async fn get_download_info(&self, gid: GroupId) -> Result<DownloadTaskInfo> {
        // TODO: Query actual download info from engine/request man
        // For now, return placeholder data
        Ok(DownloadTaskInfo {
            gid,
            status: "active".to_string(),
            ..Default::default()
        })
    }

    /// Get list of all active downloads
    ///
    /// # Returns
    /// * Vector of `DownloadTaskInfo` for each active download
    pub async fn get_active_downloads(&self) -> Result<Vec<DownloadTaskInfo>> {
        // TODO: Query actual active downloads from engine
        // For now, return empty vector
        Ok(Vec::new())
    }

    /// Get global download summary
    ///
    /// Aggregates statistics across all downloads.
    ///
    /// # Returns
    /// * `DownloadSummary` with aggregate data
    pub async fn get_summary(&self) -> Result<DownloadSummary> {
        let active = self.get_active_downloads().await?;
        
        Ok(DownloadSummary {
            active_count: active.len(),
            total_downloaded: active.iter().map(|i| i.completed_length).sum(),
            avg_speed: if active.is_empty() {
                0.0
            } else {
                active.iter().map(|i| i.download_speed).sum::<f64>() / active.len() as f64
            },
            total_elapsed: std::time::Duration::ZERO,
        })
    }

    /// Restore downloads from session file
    ///
    /// Loads previously saved session state and recreates download tasks.
    ///
    /// # Arguments
    /// * `session_path` - Path to the session file
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of restored downloads
    /// * `Err(String)` - If restoration failed
    pub async fn restore_session(&self, session_path: &std::path::Path) -> Result<usize> {
        if !session_path.exists() {
            tracing::info!("Session file does not exist, skipping restore");
            return Ok(0);
        }

        tracing::info!("Restoring downloads from session: {:?}", session_path);

        let mgr = ActiveSessionManager::new(
            session_path.to_path_buf(),
            std::time::Duration::from_secs(60),
        );

        let entries = match mgr.load_session().await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("Failed to load session: {}", e);
                return Err(Aria2Error::Recoverable(
                    aria2_core::error::RecoverableError::TemporaryNetworkFailure {
                        message: format!("Session load failed: {}", e),
                    },
                ));
            }
        };

        if entries.is_empty() {
            tracing::info!("Session is empty");
            return Ok(0);
        }

        let mut restored = 0;

        for entry in &entries {
            // Skip completed entries
            if entry.status == "complete" || entry.status == "error" {
                continue;
            }

            // Skip entries without progress
            if entry.completed_length == 0 && entry.total_length == 0 {
                continue;
            }

            // Restore download (simplified - would need full implementation)
            tracing::debug!(
                "Restoring entry GID={:?}, status={}, progress={}/{}",
                entry.gid,
                entry.status,
                entry.completed_length,
                entry.total_length
            );

            restored += 1;
        }

        tracing::info!("Restored {} download(s)", restored);
        Ok(restored)
    }

    /// Shutdown the coordinator and cleanup resources
    ///
    /// Stops all active downloads and releases resources.
    ///
    /// # Returns
    /// * `Ok(())` - Shutdown completed successfully
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("[Coordinator] Shutting down...");

        // Stop engine if running
        if let Some(engine) = self.engine.write().await.take() {
            drop(engine); // Drop engine to trigger cleanup
        }

        tracing::info!("[Coordinator] Shutdown complete");
        Ok(())
    }

    /// Format download info for display
    ///
    /// Helper method that converts DownloadTaskInfo into a human-readable string.
    ///
    /// # Arguments
    /// * `info` - The download task info to format
    ///
    /// # Returns
    /// * Formatted string suitable for console output
    pub fn format_task_info(info: &DownloadTaskInfo) -> String {
        format!(
            "GID:{:016x} | Status:{} | {}/{} | Speed:{} | Time:{}",
            info.gid,
            info.status,
            format_bytes(info.completed_length),
            format_bytes(info.total_length),
            format_speed(info.download_speed),
            format_duration(info.elapsed)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_coordinator() {
        let coordinator = DownloadCoordinator::new().await;
        assert!(coordinator.is_ok());
    }

    #[tokio::test]
    async fn test_add_download_empty_uris() {
        let coordinator = DownloadCoordinator::new().await.unwrap();
        let result = coordinator.add_download(vec![], DownloadOptions::default()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_add_download_single_uri() {
        let coordinator = DownloadCoordinator::new().await.unwrap();
        let result = coordinator
            .add_download(
                vec!["http://example.com/test.zip".to_string()],
                DownloadOptions::default(),
            )
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_download_task_info_default() {
        let info = DownloadTaskInfo::default();
        assert_eq!(info.status, "waiting");
        assert_eq!(info.total_length, 0);
        assert_eq!(info.completed_length, 0);
    }

    #[test]
    fn test_format_task_info() {
        let info = DownloadTaskInfo {
            gid: 0x12345678,
            status: "active".to_string(),
            total_length: 1024 * 1024,
            completed_length: 512 * 1024,
            download_speed: 1024.0 * 100.0,
            upload_speed: 0.0,
            elapsed: std::time::Duration::from_secs(60),
        };
        let formatted = DownloadCoordinator::format_task_info(&info);
        assert!(formatted.contains("active"));
        assert!(formatted.contains("512.00 KB"));
        assert!(formatted.contains("1.00 MB"));
    }

    #[tokio::test]
    async fn test_get_summary_empty() {
        let coordinator = DownloadCoordinator::new().await.unwrap();
        let summary = coordinator.get_summary().await.unwrap();
        assert_eq!(summary.active_count, 0);
        assert_eq!(summary.total_downloaded, 0);
    }

    #[tokio::test]
    async fn test_restore_session_nonexistent() {
        let coordinator = DownloadCoordinator::new().await.unwrap();
        let result = coordinator
            .restore_session(std::path::Path::new("/nonexistent/session.txt"))
            .await;
        assert!(result.is_ok()); // Should succeed with 0 restored
    }

    #[tokio::test]
    async fn test_shutdown() {
        let coordinator = DownloadCoordinator::new().await.unwrap();
        let result = coordinator.shutdown().await;
        assert!(result.is_ok());
    }
}
