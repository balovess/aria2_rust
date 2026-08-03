//! SFTP Download Command
//!
//! Implements the complete execution logic for SFTP downloads within the aria2
//! download engine. This command integrates the SFTP protocol layer with the
//! engine's RequestGroup, DiskWriter, and rate limiting infrastructure.
//!
//! ## Execution Flow
//!
//! ```text
//! execute()
//!   -> 1. Parse/validate URI and options
//!   -> 2. Establish SSH connection (with auth retry)
//!   -> 3. Initialize SFTP subsystem (version negotiation)
//!   -> 4. STAT remote file (get size, verify existence)
//!   -> 5. Prepare local output (create dir, open disk writer)
//!   -> 6. Chunked read loop:
//!        READ(remote_handle, offset, buf) -> WRITE(disk_writer, chunk) -> UPDATE_PROGRESS
//!   -> 7. Cleanup (close handles, disconnect)
//!   -> 8. Mark RequestGroup as complete
//! ```

mod execution;
#[cfg(test)]
mod tests;
mod types;
mod uri;

pub use types::SftpDownloadCommand;
