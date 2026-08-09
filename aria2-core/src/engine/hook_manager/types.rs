//! Core data structures for the hook system
//!
//! Defines the hook context, download statistics, and the `PostDownloadHook` trait
//! that all post-download hooks must implement.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::Result;
use crate::request::request_group::GroupId;

// Re-export DownloadStatus for backward compatibility
pub use crate::request::request_group::DownloadStatus;

// ============================================================================
// Core data structures
// ============================================================================

/// Hook execution context, containing download task status and data statistics
#[derive(Clone, Debug)]
pub struct HookContext {
    /// Unique identifier of the download task
    pub gid: GroupId,
    /// Full path of the downloaded file
    pub file_path: PathBuf,
    /// Current download status
    pub status: DownloadStatus,
    /// Download statistics
    pub stats: DownloadStats,
    /// Error message (if any)
    pub error: Option<String>,
}

impl HookContext {
    /// Create a new hook context
    ///
    /// # Arguments
    ///
    /// * `gid` - Download task group ID
    /// * `file_path` - Download file path
    /// * `status` - Download status
    /// * `stats` - Download statistics
    /// * `error` - Optional error message
    pub fn new(
        gid: GroupId,
        file_path: PathBuf,
        status: DownloadStatus,
        stats: DownloadStats,
        error: Option<String>,
    ) -> Self {
        Self {
            gid,
            file_path,
            status,
            stats,
            error,
        }
    }

    /// Get filename (without path)
    pub fn filename(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    /// Get file extension
    pub fn extension(&self) -> &str {
        self.file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
    }

    /// Get parent directory path
    pub fn directory(&self) -> &Path {
        self.file_path.parent().unwrap_or(self.file_path.as_path())
    }
}

/// Download statistics
#[derive(Clone, Debug)]
pub struct DownloadStats {
    /// Uploaded bytes
    pub uploaded_bytes: u64,
    /// Downloaded bytes
    pub downloaded_bytes: u64,
    /// Upload speed (bytes/sec)
    pub upload_speed: f64,
    /// Download speed (bytes/sec)
    pub download_speed: f64,
    /// Elapsed time (seconds)
    pub elapsed_seconds: u64,
}

impl Default for DownloadStats {
    fn default() -> Self {
        Self {
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            upload_speed: 0.0,
            download_speed: 0.0,
            elapsed_seconds: 0,
        }
    }
}

impl std::fmt::Display for DownloadStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "downloaded={}, uploaded={}, dl_speed={:.2}B/s, ul_speed={:.2}B/s, elapsed={}s",
            self.downloaded_bytes,
            self.uploaded_bytes,
            self.download_speed,
            self.upload_speed,
            self.elapsed_seconds
        )
    }
}

// ============================================================================
// PostDownloadHook trait definition
// ============================================================================

/// Post-download hook trait
///
/// Implement this trait to customize behavior after download completion.
/// All methods are async, supporting time-consuming operations in async contexts.
#[async_trait]
pub trait PostDownloadHook: Send + Sync {
    /// Callback when download completes successfully
    ///
    /// # Arguments
    ///
    /// * `context` - Context containing download task information
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, `Err(e)` on failure
    async fn on_complete(&self, context: &HookContext) -> Result<()>;

    /// Callback when download fails
    ///
    /// # Arguments
    ///
    /// * `context` - Context containing download task information
    /// * `error` - Error description string
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if error handling succeeded, `Err(e)` if error handling itself failed
    async fn on_error(&self, context: &HookContext, error: &str) -> Result<()>;

    /// Returns the hook name for logging and management
    fn name(&self) -> &'static str;
}
