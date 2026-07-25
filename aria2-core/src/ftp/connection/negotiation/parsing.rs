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
pub(super) fn parse_epsv_response(response: &str) -> Option<u16> {
    let start = response.rfind('|')?;
    let prev_pipe = response[..start].rfind('|')?;
    let port_str = &response[prev_pipe + 1..start];
    port_str.parse::<u16>().ok()
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
    let secs = days_since_epoch as u64 * 86400
        + hour as u64 * 3600
        + minute as u64 * 60
        + second as u64;
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
            if !(200..300).contains(&pass_resp.0) {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("Login failed: {} {}", pass_resp.0, pass_resp.1),
                    },
                ));
            }
            info!("FTP login successful");
        }
        _ => {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Unexpected USER response: {} {}",
                        user_resp.0, user_resp.1
                    ),
                },
            ));
        }
    }
    Ok(())
}

/// Set binary transfer mode (TYPE I) on a fresh control connection.
pub(super) async fn set_binary_mode(ctrl: &mut FreshControl) -> Result<()> {
    use tracing::debug;

    debug!("Setting transfer mode to binary (TYPE I)");
    let resp = ctrl.command("TYPE I").await?;
    if !(200..300).contains(&resp.0) {
        return Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: format!("TYPE I failed: {} {}", resp.0, resp.1),
            },
        ));
    }
    Ok(())
}

/// Query PWD to get the base working directory on a fresh control connection.
pub(super) async fn query_pwd(ctrl: &mut FreshControl) -> Result<String> {
    use tracing::debug;

    debug!("Sending PWD command");
    let resp = ctrl.command("PWD").await?;
    if resp.0 != 257 {
        return Err(Aria2Error::DownloadFailed(format!(
            "PWD command failed: {} {}",
            resp.0, resp.1
        )));
    }

    // Parse 257 "/path" current directory
    let msg = resp.1.trim();
    if let Some(start) = msg.find('"')
        && let Some(end) = msg.rfind('"')
        && end > start
    {
        Ok(msg[start + 1..end].to_string())
    } else {
        Ok(msg.to_string())
    }
}

/// CWD traversal on a fresh control connection.
///
/// Matches the C++ `sendCwdPrep` + `sendCwd`/`recvCwd` loop.
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
            return Err(Aria2Error::Recoverable(
                RecoverableError::ServerError { code: 550 },
            ));
        }
        if code != 250 {
            return Err(Aria2Error::DownloadFailed(format!(
                "CWD {} failed: {} {}",
                dir, code, msg
            )));
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
pub(super) async fn query_size(
    ctrl: &mut FreshControl,
    file_path: &str,
) -> Result<Option<u64>> {
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
pub(super) async fn send_rest(ctrl: &mut FreshControl, offset: u64) -> Result<()> {
    use tracing::{debug, warn};

    debug!("Setting resume offset: {} bytes", offset);
    let resp = ctrl.command(&format!("REST {}", offset)).await?;
    if resp.0 != 350 {
        warn!(
            "REST command not accepted by server: {} {}",
            resp.0, resp.1
        );
        // C++ aria2: CANNOT_RESUME if offset != 0 and server doesn't support REST
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    debug!("REST accepted by server");
    Ok(())
}

/// Send RETR command to start file transfer on a fresh control connection.
///
/// The `file_path` is percent-decoded before sending, matching the C++
/// `FtpConnection::sendRetr` which calls `util::percentDecode()`.
pub(super) async fn send_retr(ctrl: &mut FreshControl, file_path: &str) -> Result<()> {
    use tracing::debug;

    let decoded = percent_decode(file_path);
    debug!("Initiating file retrieval: {}", decoded);
    let resp = ctrl.command(&format!("RETR {}", decoded)).await?;
    if resp.0 == 150 || resp.0 == 125 {
        Ok(())
    } else if resp.0 == 550 {
        Err(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 550,
        }))
    } else {
        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
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
/// Each directory component is percent-decoded before sending, matching
/// the C++ `FtpConnection::sendCwd` which calls `util::percentDecode()`.
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
        let decoded = percent_decode(dir);
        debug!("CWD {}", decoded);
        let (code, msg) = ctrl.command(&format!("CWD {}", decoded)).await?;
        if code == 550 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::ServerError { code: 550 },
            ));
        }
        if code != 250 {
            return Err(Aria2Error::DownloadFailed(format!(
                "CWD {} failed: {} {}",
                decoded, code, msg
            )));
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
pub(super) async fn send_rest_pooled(ctrl: &mut PooledControl, offset: u64) -> Result<()> {
    use tracing::debug;

    debug!("Setting resume offset (pooled): {} bytes", offset);
    let resp = ctrl.command(&format!("REST {}", offset)).await?;
    if resp.0 != 350 {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    Ok(())
}

/// Send RETR command on a pooled control connection.
///
/// The `file_path` is percent-decoded before sending, matching the C++
/// `FtpConnection::sendRetr` which calls `util::percentDecode()`.
pub(super) async fn send_retr_pooled(ctrl: &mut PooledControl, file_path: &str) -> Result<()> {
    use tracing::debug;

    let decoded = percent_decode(file_path);
    debug!("Initiating file retrieval (pooled): {}", decoded);
    let resp = ctrl.command(&format!("RETR {}", decoded)).await?;
    if resp.0 == 150 || resp.0 == 125 {
        Ok(())
    } else if resp.0 == 550 {
        Err(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 550,
        }))
    } else {
        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: format!("RETR unexpected response: {} {}", resp.0, resp.1),
            },
        ))
    }
}
