//! Fresh-control FTP negotiation command helpers.

use std::time::SystemTime;

use tracing::warn;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::control::FreshControl;

use super::parsing::{cwd_targets, parse_mdtm_timestamp, parse_pwd_response};

// =============================================================================
// Fresh-control convenience wrappers (single-command helpers)
// =============================================================================

/// Send USER + PASS authentication sequence on a fresh control connection.
///
/// Error classification matches C++ aria2:
/// - 530 on USER/PASS -> `FatalError::PermissionDenied`
/// - Non-2xx/331/332 on USER -> `RecoverableError::FtpProtocolError`
/// - Non-2xx on PASS -> `FatalError::PermissionDenied` (530) or `FtpProtocolError`
pub(super) async fn authenticate(
    ctrl: &mut FreshControl,
    username: &str,
    password: &str,
) -> Result<()> {
    use tracing::{debug, info};

    debug!("Authenticating as user: {}", username);
    let user_resp = ctrl.command(&format!("USER {}", username)).await?;
    match user_resp.0 {
        230 => {
            info!("FTP login successful (no password required)");
        }
        331 | 332 => {
            debug!("Password required, sending PASS command");
            let pass_resp = ctrl.command(&format!("PASS {}", password)).await?;
            if pass_resp.0 == 530 {
                // C++ aria2: FTP_PROTOCOL_ERROR for bad login credentials
                return Err(Aria2Error::Fatal(
                    crate::error::FatalError::PermissionDenied {
                        path: format!("FTP authentication failed: {} {}", pass_resp.0, pass_resp.1),
                    },
                ));
            }
            if !(200..300).contains(&pass_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::FtpProtocolError {
                        message: format!("Login failed: {} {}", pass_resp.0, pass_resp.1),
                    },
                ));
            }
            info!("FTP login successful");
        }
        530 => {
            return Err(Aria2Error::Fatal(
                crate::error::FatalError::PermissionDenied {
                    path: format!("FTP USER rejected: {} {}", user_resp.0, user_resp.1),
                },
            ));
        }
        _ => {
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("Unexpected USER response: {} {}", user_resp.0, user_resp.1),
                },
            ));
        }
    }
    Ok(())
}

/// Set transfer mode (TYPE I or TYPE A) on a fresh control connection.
///
/// Matches C++ `FtpConnection::sendType` + `FtpNegotiationCommand::recvType`:
/// - Non-200 response -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn set_transfer_mode(ctrl: &mut FreshControl, binary: bool) -> Result<()> {
    use tracing::debug;

    let type_cmd = if binary { "TYPE I" } else { "TYPE A" };
    debug!("Setting transfer mode: {}", type_cmd);
    let resp = ctrl.command(type_cmd).await?;
    if !(200..300).contains(&resp.0) {
        // C++ aria2: EX_BAD_STATUS -> FTP_PROTOCOL_ERROR
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("{} failed: {} {}", type_cmd, resp.0, resp.1),
            },
        ));
    }
    Ok(())
}

/// Query PWD to get the base working directory on a fresh control connection.
///
/// Matches C++ `FtpNegotiationCommand::recvPwd`:
/// - Non-257 response -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn query_pwd(ctrl: &mut FreshControl) -> Result<String> {
    use tracing::debug;

    debug!("Sending PWD command");
    let resp = ctrl.command("PWD").await?;
    if resp.0 != 257 {
        return Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("PWD failed: {} {}", resp.0, resp.1),
            },
        ));
    }

    // C++ throws FTP_PROTOCOL_ERROR if no quotes are found.
    parse_pwd_response(&resp.1).ok_or_else(|| {
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError {
            message: format!("PWD response missing quoted path: {}", resp.1.trim()),
        })
    })
}

/// CWD traversal on a fresh control connection.
///
/// Matches the C++ `sendCwdPrep` + `sendCwd`/`recvCwd` loop.
///
/// Error classification matches C++:
/// - 550 -> `FatalError::FileNotFound` (C++ uses `RESOURCE_NOT_FOUND`)
/// - Non-250 (not 550) -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn cwd_traversal(
    ctrl: &mut FreshControl,
    base_working_dir: &str,
    dir_path: &str,
) -> Result<()> {
    use tracing::{debug, info};

    let dirs = cwd_targets(base_working_dir, dir_path);

    debug!("CWD traversal: {} directories to traverse", dirs.len());

    for dir in &dirs {
        debug!("CWD {}", dir);
        let (code, msg) = ctrl.command(&format!("CWD {}", dir)).await?;
        if code == 550 {
            // C++ aria2: RESOURCE_NOT_FOUND, increases file-not-found count
            return Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
                path: dir.to_string(),
            }));
        }
        if code != 250 {
            // C++ aria2: EX_BAD_STATUS -> FTP_PROTOCOL_ERROR, pools connection first
            return Err(Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: format!("CWD {} failed: {} {}", dir, code, msg),
                },
            ));
        }
    }

    info!("CWD traversal completed successfully");
    Ok(())
}

/// Query MDTM for file modification time on a fresh control connection.
pub(super) async fn query_mdtm(
    ctrl: &mut FreshControl,
    file_path: &str,
) -> Result<Option<SystemTime>> {
    use tracing::{debug, info};

    debug!("Sending MDTM command for: {}", file_path);
    let resp = ctrl.command(&format!("MDTM {}", file_path)).await?;

    if resp.0 != 213 {
        info!(
            "MDTM command returned non-213 response: {} {}",
            resp.0, resp.1
        );
        return Ok(None);
    }

    let msg = resp.1.trim();
    let timestamp_str = if let Some(stripped) = msg.strip_prefix("213") {
        stripped.trim()
    } else {
        msg
    };

    if timestamp_str.len() < 14 {
        warn!("MDTM response too short to parse: {}", timestamp_str);
        return Ok(None);
    }

    let ts = &timestamp_str[..14];
    match parse_mdtm_timestamp(ts) {
        Some(t) => {
            debug!("MDTM parsed modification time: {:?}", t);
            Ok(Some(t))
        }
        None => {
            warn!("Failed to parse MDTM timestamp: {}", ts);
            Ok(None)
        }
    }
}

/// Query SIZE for file size on a fresh control connection.
pub(super) async fn query_size(ctrl: &mut FreshControl, file_path: &str) -> Result<Option<u64>> {
    use tracing::{debug, info};

    debug!("Sending SIZE command for: {}", file_path);
    let resp = ctrl.command(&format!("SIZE {}", file_path)).await?;

    if resp.0 == 213 {
        let msg = resp.1.trim();
        let size_str = if let Some(stripped) = msg.strip_prefix("213") {
            stripped.trim()
        } else {
            msg
        };
        match size_str.parse::<u64>() {
            Ok(size) => {
                debug!("File size: {} bytes", size);
                Ok(Some(size))
            }
            Err(_) => {
                warn!("Failed to parse SIZE response: {}", size_str);
                Ok(None)
            }
        }
    } else {
        info!(
            "SIZE command returned non-213 response: {} {}",
            resp.0, resp.1
        );
        Ok(None)
    }
}

/// Send REST command for resume offset on a fresh control connection.
///
/// Matches C++ `FtpConnection::sendRest()`: always sends REST even when
/// offset is 0 (`REST 0`). Some servers require REST before RETR to
/// properly set the file pointer, and `REST 0` explicitly resets it.
pub(super) async fn send_rest(ctrl: &mut FreshControl, offset: u64) -> Result<()> {
    use tracing::{debug, warn};

    debug!("Setting resume offset: {} bytes", offset);
    // C++ always sends REST, even REST 0 (FtpConnection.cc:234-245)
    let resp = ctrl.command(&format!("REST {}", offset)).await?;
    if resp.0 != 350 {
        warn!("REST command not accepted by server: {} {}", resp.0, resp.1);
        // C++ aria2: CANNOT_RESUME if offset != 0 and server doesn't support REST
        if offset > 0 {
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }
        // REST 0 failure is non-fatal: the file pointer is already at 0
        debug!("REST 0 rejected, continuing (file pointer assumed at start)");
    } else {
        debug!("REST accepted by server");
    }
    Ok(())
}

/// Send RETR command to start file transfer on a fresh control connection.
///
/// The `file_path` is expected to already be percent-decoded (typically via
/// `extract_file_part()`), matching the C++ flow where `Request::getFile()`
/// returns a decoded path and `FtpConnection::sendRetr` applies
/// `util::percentDecode()` once. We do NOT double-decode here.
///
/// Error classification matches C++ `FtpNegotiationCommand::recvRetr`:
/// - 550 -> `FatalError::FileNotFound` (C++ uses `RESOURCE_NOT_FOUND`)
/// - Non-150/125 (not 550) -> `FtpProtocolError` (C++ uses `FTP_PROTOCOL_ERROR`)
pub(super) async fn send_retr(ctrl: &mut FreshControl, file_path: &str) -> Result<()> {
    use tracing::debug;

    debug!("Initiating file retrieval: {}", file_path);
    let resp = ctrl.command(&format!("RETR {}", file_path)).await?;
    if resp.0 == 150 || resp.0 == 125 {
        Ok(())
    } else if resp.0 == 550 {
        // C++ aria2: RESOURCE_NOT_FOUND, increases file-not-found count
        Err(Aria2Error::Fatal(crate::error::FatalError::FileNotFound {
            path: file_path.to_string(),
        }))
    } else {
        // C++ aria2: EX_BAD_STATUS -> FTP_PROTOCOL_ERROR
        Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
            },
        ))
    }
}
