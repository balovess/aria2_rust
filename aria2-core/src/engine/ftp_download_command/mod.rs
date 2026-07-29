//! FTP download command module.
//!
//! Handles the complete FTP download lifecycle including connection establishment,
//! authentication, passive/active mode negotiation, data transfer, and retry
//! logic for transient errors.

mod control;
mod execution;
mod types;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::constants;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use types::{extract_filename, parse_uri};

// ---------------------------------------------------------------------------
// FtpDownloadCommand struct
// ---------------------------------------------------------------------------

/// FTP download command that handles the complete download lifecycle.
pub struct FtpDownloadCommand {
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed_bytes: u64,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) remote_path: String,
    pub(crate) username: String,
    pub(crate) password: String,
    /// Resume offset for partial downloads (0 if not resuming)
    pub(crate) resume_offset: u64,
    /// Whether to use passive mode (true) or active mode (false)
    pub(crate) passive_mode: bool,
    /// Maximum number of retry attempts for transient errors
    pub(crate) max_retries: u32,
    /// Current retry attempt count
    pub(crate) current_retry: u32,
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

impl FtpDownloadCommand {
    /// Create a new FTP download command.
    pub fn new(
        gid: GroupId,
        uri: &str,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec![uri.to_string()],
            options.clone(),
        )));
        Self::new_with_group(group, output_dir, output_name)
    }

    /// Create an FTP download command that reuses an externally-managed
    /// `RequestGroup` (e.g. from the engine's promotion flow).
    ///
    /// The first URI in the group is used to extract FTP parameters
    /// (host, port, credentials, remote path). Output directory and filename
    /// fall back to the group's `DownloadOptions` when not explicitly
    /// overridden. The group's existing GID and progress counters are reused.
    pub fn new_with_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        output_dir: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<Self> {
        let (uri, options) = {
            let g = group.recover();
            let uri = g.uris().first().cloned().ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config(
                    "RequestGroup has no URIs for FTP download".into(),
                ))
            })?;
            let opts = g.options_arc();
            (uri, opts)
        };

        let (host, port, username, password, remote_path) = parse_uri(&uri)?;

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| constants::DEFAULT_OUTPUT_DIR.to_string());

        let filename = output_name
            .map(|n| n.to_string())
            .or_else(|| extract_filename(&remote_path))
            .unwrap_or_else(|| constants::DEFAULT_FILENAME.to_string());

        let path = std::path::PathBuf::from(&dir).join(&filename);

        // Check if file exists for resume support
        let resume_offset = if path.exists() {
            std::fs::metadata(&path)
                .ok()
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        info!(
            "FtpDownloadCommand created (shared group): {} -> {} ({}:{}/{}) [resume_offset={}]",
            uri,
            path.display(),
            host,
            port,
            remote_path,
            resume_offset
        );

        Ok(Self {
            group,
            output_path: path,
            started: false,
            completed_bytes: 0,
            host,
            port,
            remote_path,
            username,
            password,
            resume_offset,
            passive_mode: true, // Default to passive mode
            max_retries: constants::DEFAULT_MAX_RETRIES,
            current_retry: 0,
        })
    }

    /// Get read access to the request group.
    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    /// Parse FTP URI into components (delegates to `types::parse_uri`).
    pub fn parse_uri(uri: &str) -> Result<(String, u16, String, String, String)> {
        parse_uri(uri)
    }

    /// Extract filename from remote path (delegates to `types::extract_filename`).
    pub fn extract_filename(remote_path: &str) -> Option<String> {
        extract_filename(remote_path)
    }

    /// Classify FTP response code to determine error handling strategy
    /// (delegates to `types::classify_ftp_error`).
    #[allow(dead_code)] // Must remain: will be used when FTP retry-with-classification logic is integrated
    pub fn classify_ftp_error(&self, code: u16, message: &str) -> Aria2Error {
        types::classify_ftp_error(code, message, &self.host, self.port, &self.remote_path)
    }
}

// ---------------------------------------------------------------------------
// Command trait implementation (thin wrappers delegating to execution module)
// ---------------------------------------------------------------------------

#[async_trait]
impl Command for FtpDownloadCommand {
    /// Execute the FTP download with full lifecycle management.
    async fn execute(&mut self) -> Result<()> {
        execution::execute(self).await
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 || self.started {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(
            constants::FTP_DEFAULT_COMMAND_TIMEOUT_SECS,
        ))
    }
}
