//! LPD peer discovery and message parsing.
//!
//! Implements BEP 14 announcement parsing, legacy format backward
//! compatibility, and private address detection.

use std::net::IpAddr;

use tracing::debug;

use super::LpdPeer;

// =========================================================================
// LPD Announcement Parser (BEP 14)
// =========================================================================

/// Parse a raw BEP 14 LPD announcement message into structured data.
///
/// BEP 14 messages are HTTP-like:
///
/// ```http
/// BT-SEARCH * HTTP/1.1\r\n
/// Host: 239.192.152.143:6771\r\n
/// Port: 6881\r\n
/// Infohash: 0123456789abcdef0123456789abcdef01234567\r\n
/// \r\n\r\n
/// ```
///
/// The C++ parser uses `HttpHeaderProcessor(SERVER_PARSER)` to parse the
/// headers, then extracts `Infohash` and `Port` fields. We replicate this
/// with a simple line-based parser since we receive complete UDP datagrams.
///
/// # Arguments
///
/// * `data` - Raw bytes received from UDP socket
/// * `sender_ip` - IP address of the sender (from recv_from)
///
/// # Returns
///
/// `Some(LpdPeer)` if parsing succeeds, `None` if malformed
pub fn parse_lpd_announcement(data: &[u8], sender_ip: IpAddr) -> Option<LpdPeer> {
    let text = std::str::from_utf8(data).ok()?;
    let mut info_hash = String::new();
    let mut port = 0u16;

    // C++ validates the request line; we check for the BEP 14 request line
    let first_line = text.lines().next()?;
    if !first_line.starts_with("BT-SEARCH ") {
        // Also accept the old proprietary format for backward compatibility
        // during transition. This can be removed in a future release.
        return parse_lpd_announcement_legacy(data, sender_ip);
    }

    // Parse HTTP-like headers (case-insensitive matching, per HTTP spec)
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("Infohash:") {
            let val = rest.trim();
            // Validate: must be exactly 40 hex characters
            if val.len() == 40 && val.chars().all(|c| c.is_ascii_hexdigit()) {
                info_hash = val.to_lowercase(); // Normalize to lowercase
            } else {
                debug!(infohash = %val, "LPD: invalid infohash format");
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("Port:") {
            let val = rest.trim();
            match val.parse::<u16>() {
                Ok(p) if p > 0 => port = p,
                _ => {
                    debug!(port = %val, "LPD: invalid port");
                    return None;
                }
            }
        }
        // Ignore other headers (Host:, etc.) — C++ also ignores them
    }

    if !info_hash.is_empty() && port > 0 {
        debug!(
            info_hash = %&info_hash[..8],
            port,
            addr = %sender_ip,
            "Received valid BEP14 LPD announcement"
        );
        Some(LpdPeer::new(info_hash, port, sender_ip))
    } else {
        debug!(
            has_hash = !info_hash.is_empty(),
            has_port = port > 0,
            "Incomplete BEP14 LPD announcement ignored"
        );
        None
    }
}

/// Parse legacy (pre-BEP14-fix) LPD announcement format for backward compat.
///
/// Old format: `Hash: <hex>\nPort: <num>\nToken: <hex>\n`
///
/// This exists so that during the transition period, we can still understand
/// announcements from older Rust instances that haven't been updated yet.
fn parse_lpd_announcement_legacy(data: &[u8], sender_ip: IpAddr) -> Option<LpdPeer> {
    let text = std::str::from_utf8(data).ok()?;
    let mut info_hash = String::new();
    let mut port = 0u16;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Hash:") {
            let val = rest.trim();
            if val.len() == 40 && val.chars().all(|c| c.is_ascii_hexdigit()) {
                info_hash = val.to_lowercase();
            } else {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("Port:") {
            port = rest.trim().parse().ok()?;
            if port == 0 {
                return None;
            }
        }
        // Ignore Token: lines
    }

    if !info_hash.is_empty() && port > 0 {
        Some(LpdPeer::new(info_hash, port, sender_ip))
    } else {
        None
    }
}

// =========================================================================
// Private Address Detection
// =========================================================================

/// Check if an IP address is in a private/reserved range.
///
/// Matches C++ `util::inPrivateAddress()` which checks for:
/// - 10.0.0.0/8 (RFC 1918)
/// - 172.16.0.0/12 (RFC 1918)
/// - 192.168.0.0/16 (RFC 1918)
/// - 127.0.0.0/8 (loopback)
/// - 169.254.0.0/16 (link-local)
///
/// For IPv6: checks for fc00::/7 (unique local) and ::1 (loopback).
pub fn is_private_address(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 10.0.0.0/8
            octets[0] == 10
            // 172.16.0.0/12
            || (octets[0] == 172 && (octets[1] & 0xf0) == 16)
            // 192.168.0.0/16
            || (octets[0] == 192 && octets[1] == 168)
            // 127.0.0.0/8
            || octets[0] == 127
            // 169.254.0.0/16
            || (octets[0] == 169 && octets[1] == 254)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            // fc00::/7 (unique local addresses)
            (segments[0] & 0xfe00) == 0xfc00
            // ::1 (loopback)
            || v6.is_loopback()
        }
    }
}
