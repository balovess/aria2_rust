//! SFTP Transfer Protocol
//!
//! Implements high-level download and upload transfer operations over SFTP,
//! including chunked I/O, resume support, progress tracking, and rate control
//! integration.
//!
//! ## Transfer Architecture
//!
//! ```text
//! TransferOptions  ->  SftpTransfer  ->  [Chunked Read Loop]  ->  Local File
//!       |                  |                    |
//!   buffer_size      file_ops             read_at(offset, buf)
//!   resume_offset                         write(chunk)
//!   progress_cb                            update_progress()
//! ```

use super::file_ops::SftpFileOps;
use super::session::SftpSession;

mod download;
#[cfg(test)]
mod tests;
mod types;
mod upload;

pub use types::{ProgressCallback, TransferError, TransferOptions, TransferProgress};
const TRANSFER_BUF_SIZE: usize = 64 * 1024;
/// Minimum allowed buffer size (1 KB)
const MIN_BUFFER_SIZE: usize = 1024;
/// Maximum allowed buffer size (1 MB)
const MAX_BUFFER_SIZE: usize = 1024 * 1024;
/// Interval in bytes between progress reports (256 KB)
const PROGRESS_REPORT_INTERVAL: u64 = 256 * 1024;

pub struct SftpTransfer<'a> {
    /// File operations interface bound to this session
    ops: SftpFileOps<'a>,
}

impl<'a> SftpTransfer<'a> {
    /// Create a new transfer engine bound to the given SFTP session.
    pub fn new(session: &'a SftpSession) -> Self {
        Self {
            ops: SftpFileOps::new(session),
        }
    }

    // -----------------------------------------------------------------
    // Download Operation
    // -----------------------------------------------------------------

    // Utility Methods
    // -----------------------------------------------------------------

    /// Get the size of a remote file without downloading it.
    pub async fn get_remote_size(&self, remote_path: &str) -> Result<u64, TransferError> {
        let attr = self
            .ops
            .stat(remote_path)
            .await
            .map_err(|error| TransferError::remote("stat", remote_path, error))?;
        Ok(attr.size)
    }

    /// Check whether a remote file supports resume (exists and has known size).
    pub async fn check_resume_support(
        &self,
        remote_path: &str,
    ) -> Result<Option<u64>, TransferError> {
        match self.ops.stat(remote_path).await {
            Ok(attr) if attr.is_regular_file => Ok(Some(attr.size)),
            Ok(_) | Err(super::file_ops::FileOpError::NotFound { .. }) => Ok(None),
            Err(error) => Err(TransferError::remote(
                "inspect for resume",
                remote_path,
                error,
            )),
        }
    }
}

// =============================================================================
// Unit Tests
// =============================================================================
