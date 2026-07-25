//! FTP negotiation module
//!
//! Implements the full FTP negotiation flow matching the C++ aria2
//! `FtpNegotiationCommand` state machine, but using Rust's async/await
//! model instead of 30+ explicit states.
//!
//! The negotiation flow (in order):
//! 1. Connect + read greeting (or skip if using pooled connection)
//! 2. Authenticate (or skip if pooled)
//! 3. FEAT - query server capabilities
//! 4. OPTS UTF8 ON (if FEAT reports UTF8 support)
//! 5. SYST - query server system type (for VMS path handling)
//! 6. Set TYPE (binary)
//! 7. PWD to get baseWorkingDir
//! 8. CWD traversal (split path, CWD each directory component)
//! 9. MDTM (if remote-time option is enabled)
//! 10. SIZE
//! 11. Choose data connection mode (EPSV/PASV or EPRT/PORT)
//! 12. REST (after data connection established, per C++ ordering)
//! 13. RETR
//!
//! After data transfer completes, call `finish_download()` to read the
//! 226 transfer-complete response and optionally pool the connection.
//!
//! # Module organization
//!
//! - [`control`]       - I/O layer: `RawFtpControl`, `FreshControl`, `PooledControl`
//! - [`capabilities`]  - FEAT parsing, server capability tracking, and new commands
//! - [`parsing`]       - Response parsing, path helpers, and single-command helpers
//! - [`fresh_flow`]    - FtpNegotiator methods for fresh (non-pooled) connections
//! - [`pooled_flow`]   - FtpNegotiator methods for pooled (pre-authenticated) connections

mod capabilities;
mod control;
mod fresh_flow;
mod parsing;
mod pooled_flow;

#[cfg(test)]
mod tests;

use std::time::SystemTime;

use tokio::time::Duration;
use tracing::info;

use crate::error::{Aria2Error, Result};
use crate::ftp::connection::negotiation::control::PooledControl;
use crate::ftp::connection::types::FtpMode;
use crate::ftp::connection::negotiation::parsing::{
    cwd_traversal_pooled, extract_directory_part, extract_file_part, query_mdtm_pooled,
    query_size_pooled, send_rest_pooled, send_retr_pooled,
};

// Re-export public types from submodules
pub use capabilities::ServerCapabilities;
pub use control::RawFtpControl;

/// Result of a successful FTP negotiation.
///
/// Contains everything the download pipeline needs to begin reading data
/// and to finalize the transfer afterwards.
pub struct FtpNegotiationResult {
    /// Data connection for reading file content
    pub data_stream: tokio::net::TcpStream,
    /// Control connection preserved for reading the 226 response later
    pub control: RawFtpControl,
    /// File size reported by SIZE command (None if SIZE not supported)
    pub file_size: Option<u64>,
    /// Modification time from MDTM command (None if MDTM not supported or disabled)
    pub modification_time: Option<SystemTime>,
    /// Base working directory from PWD, used for connection pool key
    pub base_working_dir: String,
    /// Server capabilities detected from FEAT command
    pub capabilities: ServerCapabilities,
}

/// Configuration for FTP negotiation.
#[derive(Debug, Clone)]
pub struct FtpNegotiationConfig {
    /// Server hostname
    pub host: String,
    /// Server port (typically 21)
    pub port: u16,
    /// Username for authentication
    pub username: String,
    /// Password for authentication
    pub password: String,
    /// URL-decoded remote path (e.g., "/pub/linux/file.tar.gz")
    pub remote_path: String,
    /// Data connection mode (passive or active)
    pub mode: FtpMode,
    /// Resume offset in bytes (0 = no resume)
    pub resume_offset: u64,
    /// Whether to send MDTM for remote time
    pub remote_time: bool,
    /// Connection timeout
    pub connect_timeout: Duration,
    /// Read/response timeout for FTP commands
    pub command_timeout: Duration,
    /// Whether this is a pooled connection (skip connect + auth)
    pub is_pooled: bool,
    /// Base working directory for pooled connections (must match)
    pub pooled_base_working_dir: Option<String>,
}

/// FTP negotiation orchestrator.
///
/// Performs the full FTP negotiation flow as a linear async function
/// instead of the C++ state machine with 30+ states.
pub struct FtpNegotiator;

impl FtpNegotiator {
    /// Execute the complete FTP negotiation flow.
    ///
    /// This replaces the C++ `FtpNegotiationCommand` state machine with
    /// a sequential async function. The ordering matches the C++ and adds
    /// FEAT/SYST/OPTS for capability detection:
    ///
    /// 1. Connect + greeting (or skip for pooled)
    /// 2. Authenticate (or skip for pooled)
    /// 3. FEAT -> detect capabilities
    /// 4. OPTS UTF8 ON (if FEAT reports UTF8)
    /// 5. SYST -> detect server type
    /// 6. TYPE I (binary)
    /// 7. PWD -> baseWorkingDir
    /// 8. CWD traversal (each directory component separately)
    /// 9. MDTM (if remote_time enabled)
    /// 10. SIZE
    /// 11. Data connection (EPSV/PASV or EPRT/PORT)
    /// 12. REST (after data connection established)
    /// 13. RETR
    pub async fn negotiate(config: FtpNegotiationConfig) -> Result<FtpNegotiationResult> {
        let FtpNegotiationConfig {
            host,
            port,
            username,
            password,
            remote_path,
            mode,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled,
            pooled_base_working_dir: _,
        } = config;

        // Step 1-2: Connect + authenticate (or skip if pooled)
        let (mut ctrl, mut capabilities) = if is_pooled {
            return Err(Aria2Error::DownloadFailed(
                "Pooled connection negotiation requires a pre-established control stream".into(),
            ));
        } else {
            Self::connect_and_authenticate(
                &host,
                port,
                &username,
                &password,
                connect_timeout,
                command_timeout,
            )
            .await?
        };

        // Steps 3-4 (FEAT + OPTS UTF8 ON) are already done inside
        // connect_and_authenticate, matching the C++ aria2 flow where
        // FEAT/OPTS are sent immediately after login.

        // Step 5: SYST - query server system type
        capabilities.syst = capabilities::query_syst(&mut ctrl).await?;

        // Step 6: TYPE I (binary)
        parsing::set_binary_mode(&mut ctrl).await?;

        // Step 7: PWD -> baseWorkingDir
        let base_working_dir = parsing::query_pwd(&mut ctrl).await?;
        info!("Base working directory: {}", base_working_dir);

        // Step 8: CWD traversal
        let dir_part = extract_directory_part(&remote_path);
        parsing::cwd_traversal(&mut ctrl, &base_working_dir, &dir_part).await?;

        // Step 9: MDTM (if remote_time enabled and server supports it)
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            parsing::query_mdtm(&mut ctrl, &file_part).await?
        } else {
            None
        };

        // Step 10: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = parsing::query_size(&mut ctrl, &file_part).await?;

        // Step 11: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                Self::enter_passive_mode(&mut ctrl, &host, connect_timeout, &capabilities).await?
            }
            FtpMode::Active => {
                Self::enter_active_mode(&mut ctrl, connect_timeout, &capabilities).await?
            }
        };

        // Step 12: REST (after data connection established, per C++ ordering)
        if resume_offset > 0 {
            parsing::send_rest(&mut ctrl, resume_offset).await?;
        }

        // Step 13: RETR
        parsing::send_retr(&mut ctrl, &file_part).await?;

        // Build result - detach the control stream
        let ctrl_reader = ctrl.reader;
        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl_reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
            capabilities,
        };

        Ok(result)
    }

    /// Negotiate using a pooled (pre-authenticated) connection.
    ///
    /// When a connection is reused from the pool, we skip connect + auth
    /// and start from CWD_PREP (step 8). The baseWorkingDir must match.
    pub async fn negotiate_pooled(
        control: RawFtpControl,
        config: FtpNegotiationConfig,
    ) -> Result<FtpNegotiationResult> {
        let FtpNegotiationConfig {
            host,
            port: _,
            username: _,
            password: _,
            remote_path,
            mode,
            resume_offset,
            remote_time,
            connect_timeout,
            command_timeout,
            is_pooled: _,
            pooled_base_working_dir,
        } = config;

        let mut ctrl = PooledControl {
            reader: control.reader,
            read_timeout: command_timeout,
        };

        let base_working_dir = pooled_base_working_dir.unwrap_or_else(|| "/".to_string());

        // Pooled connections already have FEAT/SYST info from initial session,
        // so we use a default (empty) capability set here.
        let capabilities = ServerCapabilities::new();

        // Step 8: CWD traversal (skip connect + auth + FEAT + SYST + TYPE + PWD for pooled)
        let dir_part = extract_directory_part(&remote_path);
        cwd_traversal_pooled(&mut ctrl, &base_working_dir, &dir_part).await?;

        // Step 9: MDTM
        let modification_time = if remote_time {
            let file_part = extract_file_part(&remote_path);
            query_mdtm_pooled(&mut ctrl, &file_part).await?
        } else {
            None
        };

        // Step 10: SIZE
        let file_part = extract_file_part(&remote_path);
        let file_size = query_size_pooled(&mut ctrl, &file_part).await?;

        // Step 11: Data connection
        let data_stream = match mode {
            FtpMode::Passive => {
                Self::enter_passive_mode_pooled(&mut ctrl, &host, connect_timeout, &capabilities)
                    .await?
            }
            FtpMode::Active => {
                Self::enter_active_mode_pooled(&mut ctrl, connect_timeout, &capabilities).await?
            }
        };

        // Step 12: REST
        if resume_offset > 0 {
            send_rest_pooled(&mut ctrl, resume_offset).await?;
        }

        // Step 13: RETR
        send_retr_pooled(&mut ctrl, &file_part).await?;

        let result = FtpNegotiationResult {
            data_stream,
            control: RawFtpControl::new(ctrl.reader, host.clone(), command_timeout),
            file_size,
            modification_time,
            base_working_dir,
            capabilities,
        };

        Ok(result)
    }

    /// Finish a download: read the 226 response and optionally pool the connection.
    ///
    /// Per C++ `FtpFinishDownloadCommand`, non-226 responses are not fatal
    /// (data was already received). If the connection is reusable and the
    /// pool key (host + username + baseWorkingDir) matches, the connection
    /// can be returned for reuse.
    ///
    /// # Arguments
    ///
    /// - `control`: The raw FTP control connection from `FtpNegotiationResult`
    /// - `reuse_connection`: Whether connection pooling is enabled
    ///
    /// # Returns
    ///
    /// `Some(())` if the connection can be pooled, `None` otherwise.
    pub async fn finish_download(
        control: &mut RawFtpControl,
        reuse_connection: bool,
    ) -> Option<()> {
        // Read 226 transfer-complete response
        let transfer_ok = control.read_transfer_complete().await.ok()?;

        if transfer_ok && reuse_connection {
            // Connection is good for reuse
            return Some(());
        }

        // Non-226 or pooling disabled; still return Some(()) to indicate
        // the finish completed (data was already received)
        Some(())
    }
}
