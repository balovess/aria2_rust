//! Pooled-control FTP negotiation command helpers.

use std::time::SystemTime;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::control::PooledControl;

use super::parsing::{cwd_targets, parse_mdtm_timestamp};

// =============================================================================
// Pooled-control convenience wrappers (single-command helpers)
// =============================================================================

/// CWD traversal on a pooled control connection.
///
/// The `dir_path` is expected to already be percent-decoded (typically via
/// `extract_directory_part()`), matching the fresh-connection `cwd_traversal`
/// behavior. We do NOT double-decode here.
///
/// Error classification matches C++:
/// - 550 -> `FatalError::FileNotFound` (C++ uses `RESOURCE_NOT_FOUND`)
/// - Non-250 (not 550) -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn cwd_traversal_pooled(
    ctrl: &mut PooledControl,
    base_working_dir: &str,
    dir_path: &str,
) -> Result<()> {
    use tracing::{debug, info};

    let dirs = cwd_targets(base_working_dir, dir_path);

    debug!("CWD traversal (pooled): {} directories", dirs.len());
    for dir in &dirs {
        debug!("CWD {}", dir);
        let (code, msg) = ctrl.command(&format!("CWD {}", dir)).await?;
        if code == 550 {
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: dir.to_string(),
            }));
        }
        if code != 250 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("CWD {} failed (pooled): {} {}", dir, code, msg),
                },
            ));
        }
    }
    info!("CWD traversal (pooled) completed successfully");
    Ok(())
}

/// Query MDTM for file modification time on a pooled control connection.
pub(super) async fn query_mdtm_pooled(
    ctrl: &mut PooledControl,
    file_path: &str,
) -> Result<Option<SystemTime>> {
    use tracing::{debug, info};

    debug!("Sending MDTM command (pooled) for: {}", file_path);
    let resp = ctrl.command(&format!("MDTM {}", file_path)).await?;

    if resp.0 != 213 {
        info!("MDTM non-213: {} {}", resp.0, resp.1);
        return Ok(None);
    }

    let msg = resp.1.trim();
    let timestamp_str = if let Some(stripped) = msg.strip_prefix("213") {
        stripped.trim()
    } else {
        msg
    };
    if timestamp_str.len() < 14 {
        return Ok(None);
    }
    match parse_mdtm_timestamp(&timestamp_str[..14]) {
        Some(t) => Ok(Some(t)),
        None => Ok(None),
    }
}

/// Query SIZE for file size on a pooled control connection.
pub(super) async fn query_size_pooled(
    ctrl: &mut PooledControl,
    file_path: &str,
) -> Result<Option<u64>> {
    use tracing::debug;

    debug!("Sending SIZE command (pooled) for: {}", file_path);
    let resp = ctrl.command(&format!("SIZE {}", file_path)).await?;

    if resp.0 == 213 {
        let msg = resp.1.trim();
        let size_str = if let Some(stripped) = msg.strip_prefix("213") {
            stripped.trim()
        } else {
            msg
        };
        Ok(size_str.parse::<u64>().ok())
    } else {
        Ok(None)
    }
}

/// Send REST command for resume offset on a pooled control connection.
///
/// Matches C++ `FtpConnection::sendRest()`: always sends REST even when
/// offset is 0. REST 0 rejection is non-fatal (file pointer is already
/// at position 0).
pub(super) async fn send_rest_pooled(ctrl: &mut PooledControl, offset: u64) -> Result<()> {
    use tracing::debug;

    debug!("Setting resume offset (pooled): {} bytes", offset);
    let resp = ctrl.command(&format!("REST {}", offset)).await?;
    if resp.0 != 350 {
        if offset > 0 {
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }
        // REST 0 rejection is non-fatal
        debug!("REST 0 rejected (pooled), continuing");
    }
    Ok(())
}

/// Send RETR command on a pooled control connection.
///
/// The `file_path` is expected to already be percent-decoded (typically via
/// `extract_file_part()`), matching the fresh-connection `send_retr`
/// behavior. We do NOT double-decode here.
///
/// Error classification matches C++:
/// - 550 -> `FatalError::FileNotFound` (C++ uses `RESOURCE_NOT_FOUND`)
/// - Non-150/125 (not 550) -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn send_retr_pooled(ctrl: &mut PooledControl, file_path: &str) -> Result<()> {
    use tracing::debug;

    debug!("Initiating file retrieval (pooled): {}", file_path);
    let resp = ctrl.command(&format!("RETR {}", file_path)).await?;
    if resp.0 == 150 || resp.0 == 125 {
        Ok(())
    } else if resp.0 == 550 {
        Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
            path: file_path.to_string(),
        }))
    } else {
        Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("RETR unexpected response (pooled): {} {}", resp.0, resp.1),
            },
        ))
    }
}
