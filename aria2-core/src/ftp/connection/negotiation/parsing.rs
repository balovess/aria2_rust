//! FTP response parsing and path manipulation helpers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::connection::negotiation::control::FreshControl;
use crate::ftp::connection::negotiation::control::PooledControl;

// =============================================================================
// Path manipulation helpers
// =============================================================================

/// Percent-decode a string, handling UTF-8 multi-byte sequences correctly.
///
/// Matches the C++ `util::percentDecode()` applied to CWD/RETR paths.
/// For example, `%E6%96%87%E4%BB%B6` decodes to the Chinese character for "file".
///
/// This is public within the crate so that CWD and RETR command senders
/// can decode URL-encoded paths before sending them to the FTP server,
/// matching the C++ `FtpConnection::sendCwd` and `sendRetr` behavior
/// which call `util::percentDecode()` on every path argument.
pub(crate) fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                // Invalid percent-encoding, preserve literal characters
                bytes.push(b'%');
                bytes.extend_from_slice(hex.as_bytes());
            }
        } else {
            bytes.push(c as u8);
        }
    }
    // Decode the full byte sequence as UTF-8, with lossy fallback for invalid sequences
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Extract the directory part of a remote path, with percent-decoding.
///
/// For `/pub/linux/file.tar.gz`, returns `/pub/linux`.
/// For `/file.txt`, returns `` (empty, meaning no CWD needed).
/// The file name (last component) is NOT included as a CWD target.
///
/// Percent-encoded sequences in the path are decoded before returning,
/// matching the C++ `util::percentDecode()` applied before CWD commands.
pub(super) fn extract_directory_part(remote_path: &str) -> String {
    if remote_path.is_empty() {
        return String::new();
    }
    let decoded = percent_decode(remote_path);
    match decoded.rfind('/') {
        Some(idx) => decoded[..idx].to_string(),
        None => String::new(),
    }
}

/// Extract the file name part of a remote path, with percent-decoding.
///
/// For `/pub/linux/file.tar.gz`, returns `file.tar.gz`.
/// For `/file.txt`, returns `file.txt`.
/// For `/`, returns `` (empty).
///
/// Percent-encoded sequences in the file name are decoded before returning,
/// matching the C++ `util::percentDecode()` applied before RETR commands.
pub(super) fn extract_file_part(remote_path: &str) -> String {
    if remote_path.is_empty() {
        return String::new();
    }
    let decoded = percent_decode(remote_path);
    match decoded.rfind('/') {
        Some(idx) => decoded[idx + 1..].to_string(),
        None => decoded,
    }
}

// =============================================================================
// Response parsing helpers
// =============================================================================

/// Parse PASV response to extract IP and port.
pub(super) fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
    let start = response.find('(')?;
    let end = response.rfind(')')?;
    let inner = &response[start + 1..end];
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 6 {
        return None;
    }
    let h1: u8 = parts[0].trim().parse().ok()?;
    let h2: u8 = parts[1].trim().parse().ok()?;
    let h3: u8 = parts[2].trim().parse().ok()?;
    let h4: u8 = parts[3].trim().parse().ok()?;
    let p1: u16 = parts[4].trim().parse().ok()?;
    let p2: u16 = parts[5].trim().parse().ok()?;
    Some((format!("{}.{}.{}.{}", h1, h2, h3, h4), p1 * 256 + p2))
}

/// Parse EPSV response to extract port.
///
/// Matches C++ `FtpConnection::receiveEpsvResponse()`: parses the
/// `(|<net>|<proto>|<port>|)` format by finding the parenthesized portion
/// (or the raw `|||port|` pattern), splitting on `|`, and extracting the
/// port from the 4th field. The port must be in range 1..=65535 (0 is
/// rejected per C++).
pub(super) fn parse_epsv_response(response: &str) -> Option<u16> {
    // Try to find the parenthesized portion first: (|...|port|)
    let epsv_part = if let Some(open) = response.find('(') {
        let close = response.rfind(')').filter(|&c| c > open)?;
        &response[open + 1..close]
    } else {
        // No parentheses — use the whole string (e.g., "|||60000|")
        response
    };

    // Split on '|' keeping empty segments.
    // Format: |net|proto|port| → ["", "net", "proto", "port", ""]
    // Or:     |||port|       → ["", "", "", "port", ""]
    let parts: Vec<&str> = epsv_part.split('|').collect();

    // Need at least 5 segments: empty, net, proto, port, empty/trailing
    if parts.len() < 5 {
        return None;
    }

    // Port is the 4th segment (index 3)
    let port_str = parts[3];
    let port: u16 = port_str.parse().ok()?;

    // C++ validates 0 < port <= UINT16_MAX
    if port == 0 {
        return None;
    }

    Some(port)
}

/// Parse MDTM timestamp `YYYYMMDDhhmmss` to `SystemTime` (UTC).
pub(super) fn parse_mdtm_timestamp(s: &str) -> Option<SystemTime> {
    if s.len() < 14 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    let hour: u32 = s[8..10].parse().ok()?;
    let minute: u32 = s[10..12].parse().ok()?;
    let second: u32 = s[12..14].parse().ok()?;

    if !(1990..=2999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let days_since_epoch = days_from_civil(year, month, day)?;
    let secs =
        days_since_epoch as u64 * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since 1970-01-01 using Howard Hinnant's civil_from_days algorithm.
pub(super) fn days_from_civil(year: i32, month: u32, day: u32) -> Option<u64> {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let doy = (153 * m as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days)
}

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

/// Set binary transfer mode (TYPE I) on a fresh control connection.
///
/// Convenience wrapper around [`set_transfer_mode`].
#[allow(dead_code)] // Kept for backward compatibility; prefer set_transfer_mode
pub(super) async fn set_binary_mode(ctrl: &mut FreshControl) -> Result<()> {
    set_transfer_mode(ctrl, true).await
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

    // Parse 257 "/path" current directory
    let msg = resp.1.trim();
    if let Some(start) = msg.find('"')
        && let Some(end) = msg.rfind('"')
        && end > start
    {
        Ok(msg[start + 1..end].to_string())
    } else {
        // C++ throws FTP_PROTOCOL_ERROR if no quotes found
        Err(Aria2Error::Recoverable(
            RecoverableError::FtpProtocolError {
                message: format!("PWD response missing quoted path: {}", msg),
            },
        ))
    }
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

    // Build the CWD queue: baseWorkingDir first, then each dir component
    let mut dirs: Vec<&str> = Vec::new();

    // Add base working dir if not root
    if base_working_dir != "/" && !base_working_dir.is_empty() {
        dirs.push(base_working_dir);
    }

    // Split directory path into components (skip empty segments from //)
    for component in dir_path.split('/') {
        if !component.is_empty() {
            dirs.push(component);
        }
    }

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
    let timestamp_str = if msg.starts_with("213") {
        msg[3..].trim()
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
        let size_str = if msg.starts_with("213") {
            msg[3..].trim()
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

    let mut dirs: Vec<&str> = Vec::new();
    if base_working_dir != "/" && !base_working_dir.is_empty() {
        dirs.push(base_working_dir);
    }
    for component in dir_path.split('/') {
        if !component.is_empty() {
            dirs.push(component);
        }
    }

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
    let timestamp_str = if msg.starts_with("213") {
        msg[3..].trim()
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
        let size_str = if msg.starts_with("213") {
            msg[3..].trim()
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
