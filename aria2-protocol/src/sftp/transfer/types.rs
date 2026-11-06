//! Public SFTP transfer options, errors, and progress values.

use std::io;
use std::path::PathBuf;

use super::super::file_ops::FileOpError;
use super::{MAX_BUFFER_SIZE, MIN_BUFFER_SIZE, TRANSFER_BUF_SIZE};
/// Progress callback signature: receives (bytes_transferred, total_bytes, speed_bps)
pub type ProgressCallback = Box<dyn Fn(u64, u64, f64) + Send + Sync>;

/// Failure returned by a standalone SFTP transfer.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("SFTP transfer cancelled")]
    Cancelled,
    #[error("remote SFTP operation {operation} failed for {path}: {source}")]
    RemoteOperation {
        operation: String,
        path: String,
        #[source]
        source: FileOpError,
    },
    #[error("local I/O operation {operation} failed for {path}: {source}")]
    LocalIo {
        operation: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("remote path is not a regular file: {path} ({file_type})")]
    NotRegularFile {
        path: String,
        file_type: &'static str,
    },
}

impl TransferError {
    pub(super) fn remote(
        operation: impl Into<String>,
        path: impl Into<String>,
        source: FileOpError,
    ) -> Self {
        Self::RemoteOperation {
            operation: operation.into(),
            path: path.into(),
            source,
        }
    }

    pub(super) fn local(
        operation: impl Into<String>,
        path: impl Into<PathBuf>,
        source: io::Error,
    ) -> Self {
        Self::LocalIo {
            operation: operation.into(),
            path: path.into(),
            source,
        }
    }
}

/// Configuration options for SFTP transfer operations.
pub struct TransferOptions {
    /// Size of each read/write buffer (default: 64KB, range: 1KB-1MB)
    pub buffer_size: usize,
    /// Starting byte offset for resume/partial downloads
    pub resume_offset: u64,
    /// Whether to preserve source file permissions at the destination.
    pub preserve_permissions: bool,
    /// Whether to preserve source file access and modification times.
    pub preserve_time: bool,
    /// Optional progress callback invoked periodically during transfer
    pub progress_callback: Option<ProgressCallback>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            buffer_size: TRANSFER_BUF_SIZE,
            resume_offset: 0,
            preserve_permissions: false,
            preserve_time: false,
            progress_callback: None,
        }
    }
}

impl TransferOptions {
    /// Set the resume offset for partial download continuation.
    pub fn with_resume(mut self, offset: u64) -> Self {
        self.resume_offset = offset;
        self
    }

    /// Set a custom buffer size (clamped to valid range).
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size.clamp(MIN_BUFFER_SIZE, MAX_BUFFER_SIZE);
        self
    }

    /// Enable preservation of remote file metadata (permissions + timestamps).
    pub fn preserve_metadata(mut self) -> Self {
        self.preserve_permissions = true;
        self.preserve_time = true;
        self
    }

    /// Register a progress callback that will be called periodically.
    pub fn with_progress_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(u64, u64, f64) + Send + Sync + 'static,
    {
        self.progress_callback = Some(Box::new(cb));
        self
    }
}

// =============================================================================
// Transfer Progress
// =============================================================================

/// Represents the current state of an in-progress or completed transfer.
#[derive(Debug, Clone)]
pub struct TransferProgress {
    /// Total bytes transferred so far
    pub bytes_transferred: u64,
    /// Total expected bytes (file size, 0 if unknown)
    pub total_bytes: u64,
    /// Current transfer speed in bytes per second
    pub speed_bytes_per_sec: f64,
    /// Elapsed time since transfer started (seconds)
    pub elapsed_secs: f64,
}

impl TransferProgress {
    /// Calculate completion percentage (100.0 if total is unknown/zero).
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            100.0
        } else {
            (self.bytes_transferred as f64 / self.total_bytes as f64) * 100.0
        }
    }

    /// Check if the transfer is complete (transferred >= total or total unknown).
    pub fn is_complete(&self) -> bool {
        self.bytes_transferred >= self.total_bytes || self.total_bytes == 0
    }

    /// Get remaining bytes to transfer.
    pub fn remaining(&self) -> u64 {
        self.total_bytes.saturating_sub(self.bytes_transferred)
    }

    /// Estimated time remaining in seconds (based on current speed).
    pub fn eta_secs(&self) -> Option<f64> {
        let remaining = self.remaining();
        if self.speed_bytes_per_sec > 0.0 && remaining > 0 {
            Some(remaining as f64 / self.speed_bytes_per_sec)
        } else {
            None
        }
    }
}

impl std::fmt::Display for TransferProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1}% ({}/{} @ {:.1} KB/s, {:.1}s elapsed",
            self.percent(),
            self.bytes_transferred,
            if self.total_bytes > 0 {
                format!("{}", self.total_bytes)
            } else {
                "?".to_string()
            },
            self.speed_bytes_per_sec / 1024.0,
            self.elapsed_secs
        )?;
        if let Some(eta) = self.eta_secs() {
            write!(f, ", ETA: {:.1}s", eta)?;
        }
        Ok(())
    }
}

// =============================================================================
// SFTP Transfer Engine
// =============================================================================
