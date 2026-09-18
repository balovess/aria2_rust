//! Stateless FTP path and response parsers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
            let mut encoded = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
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
    split_remote_path(remote_path).0
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
    split_remote_path(remote_path).1
}

/// Split a URL-encoded FTP path into decoded directory and file parts.
pub(crate) fn split_remote_path(remote_path: &str) -> (String, String) {
    split_decoded_remote_path(&percent_decode(remote_path))
}

/// Split an already-decoded FTP path into directory and file parts.
///
/// The production download command decodes its URI once while constructing
/// the request. Keeping this variant explicit prevents a second percent
/// decode when the command later switches to the FTP working directory.
pub(crate) fn split_decoded_remote_path(remote_path: &str) -> (String, String) {
    if remote_path.is_empty() {
        return (String::new(), String::new());
    }

    match remote_path.rfind('/') {
        Some(idx) => (
            remote_path[..idx].to_string(),
            remote_path[idx + 1..].to_string(),
        ),
        None => (String::new(), remote_path.to_string()),
    }
}

/// Return the CWD commands required by the original FTP negotiation order.
///
/// The base working directory is one command, followed by each non-empty URI
/// directory component. This preserves the original command sequence while
/// keeping path traversal independent from any particular control adapter.
pub(crate) fn cwd_targets(base_working_dir: &str, dir_path: &str) -> Vec<String> {
    let mut dirs = Vec::new();
    if !base_working_dir.is_empty() {
        dirs.push(base_working_dir.to_string());
    }
    dirs.extend(
        dir_path
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned),
    );
    dirs
}

/// Parse the quoted path from a successful FTP `PWD` response.
pub(crate) fn parse_pwd_response(response: &str) -> Option<String> {
    let message = response.trim();
    let start = message.find('"')?;
    let end = message.rfind('"')?;
    (end > start).then(|| message[start + 1..end].to_string())
}

// =============================================================================
// Response parsing helpers
// =============================================================================

/// Parse PASV response to extract IP and port.
pub(crate) fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
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
    let p1: u8 = parts[4].trim().parse().ok()?;
    let p2: u8 = parts[5].trim().parse().ok()?;
    Some((
        format!("{}.{}.{}.{}", h1, h2, h3, h4),
        u16::from(p1) * 256 + u16::from(p2),
    ))
}

/// Parse EPSV response to extract port.
///
/// Matches C++ `FtpConnection::receiveEpsvResponse()`: parses the
/// `(|<net>|<proto>|<port>|)` format by finding the parenthesized portion
/// (or the raw `|||port|` pattern), splitting on `|`, and extracting the
/// port from the 4th field. The port must be in range 1..=65535 (0 is
/// rejected per C++).
pub(crate) fn parse_epsv_response(response: &str) -> Option<u16> {
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
pub(crate) fn parse_mdtm_timestamp(s: &str) -> Option<SystemTime> {
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
    let secs = days_since_epoch * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
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
