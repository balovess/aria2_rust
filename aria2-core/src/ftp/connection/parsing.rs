//! Stateless FTP path and response parsers shared by the download engine.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Percent-decode a path while preserving invalid escape sequences.
pub(crate) fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                bytes.push(b'%');
                bytes.extend_from_slice(hex.as_bytes());
            }
        } else {
            let mut encoded = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Split an already-decoded FTP path into directory and file parts.
pub(crate) fn split_decoded_remote_path(remote_path: &str) -> (String, String) {
    if remote_path.is_empty() {
        return (String::new(), String::new());
    }

    match remote_path.rfind('/') {
        Some(index) => (
            remote_path[..index].to_string(),
            remote_path[index + 1..].to_string(),
        ),
        None => (String::new(), remote_path.to_string()),
    }
}

/// Return the CWD commands required by the aria2 FTP command order.
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

/// Parse a PASV response to extract the IPv4 address and port.
pub(crate) fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
    let start = response.find('(')?;
    let end = response.rfind(')')?;
    let parts: Vec<&str> = response[start + 1..end].split(',').collect();
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

/// Parse an EPSV response and return its data port.
pub(crate) fn parse_epsv_response(response: &str) -> Option<u16> {
    let epsv_part = if let Some(open) = response.find('(') {
        let close = response.rfind(')').filter(|&close| close > open)?;
        &response[open + 1..close]
    } else {
        response
    };
    let parts: Vec<&str> = epsv_part.split('|').collect();
    if parts.len() < 5 {
        return None;
    }

    let port: u16 = parts[3].parse().ok()?;
    (port != 0).then_some(port)
}

/// Parse an MDTM timestamp (`YYYYMMDDhhmmss`) as UTC.
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
    let seconds =
        days_since_epoch * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<u64> {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let doy = (153 * m as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_utf8_and_invalid_escapes() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E6%96%87%E4%BB%B6"), "文件");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
    }

    #[test]
    fn split_path_does_not_decode_twice() {
        assert_eq!(
            split_decoded_remote_path("/pub/linux/file.tar.gz"),
            ("/pub/linux".to_string(), "file.tar.gz".to_string())
        );
        assert_eq!(
            split_decoded_remote_path("file.txt"),
            (String::new(), "file.txt".to_string())
        );
    }

    #[test]
    fn cwd_targets_preserve_base_and_skip_empty_components() {
        assert_eq!(
            cwd_targets("/srv", "/pub//linux"),
            vec!["/srv", "pub", "linux"]
        );
    }

    #[test]
    fn parse_pwd_response_requires_quotes() {
        assert_eq!(
            parse_pwd_response("257 \"/pub\" is current"),
            Some("/pub".into())
        );
        assert_eq!(parse_pwd_response("257 /pub"), None);
    }

    #[test]
    fn parse_pasv_response_validates_six_octets() {
        assert_eq!(
            parse_pasv_response("227 Entering Passive Mode (192,168,1,2,195,80)"),
            Some(("192.168.1.2".into(), 50000))
        );
        assert_eq!(parse_pasv_response("227 (192,168,1,2,195)"), None);
    }

    #[test]
    fn parse_epsv_response_accepts_standard_forms() {
        assert_eq!(
            parse_epsv_response("229 Entering Extended Passive Mode (|||50001|)"),
            Some(50001)
        );
        assert_eq!(parse_epsv_response("|||60000|"), Some(60000));
        assert_eq!(parse_epsv_response("|||0|"), None);
    }

    #[test]
    fn parse_mdtm_timestamp_validates_ranges() {
        assert!(parse_mdtm_timestamp("20240115103000").is_some());
        assert!(parse_mdtm_timestamp("20241315103000").is_none());
        assert!(parse_mdtm_timestamp("short").is_none());
    }
}
