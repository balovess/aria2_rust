//! FTP response and remote-path parsing shared by FTP clients.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use super::super::uri::percent_decode;

pub fn split_decoded_remote_path(path: &str) -> (String, String) {
    if path.is_empty() {
        return (String::new(), String::new());
    }
    match path.rfind('/') {
        Some(index) => (path[..index].to_owned(), path[index + 1..].to_owned()),
        None => (String::new(), path.to_owned()),
    }
}

pub fn split_remote_path(path: &str) -> (String, String) {
    split_decoded_remote_path(&percent_decode(path))
}

pub fn cwd_targets(base_working_dir: &str, directory: &str) -> Vec<String> {
    let mut targets = Vec::new();
    if !base_working_dir.is_empty() {
        targets.push(base_working_dir.to_owned());
    }
    targets.extend(
        directory
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned),
    );
    targets
}

pub fn parse_pwd_response(response: &str) -> Option<String> {
    let message = response.trim();
    let start = message.find('"')?;
    let end = message.rfind('"')?;
    (end > start).then(|| message[start + 1..end].to_owned())
}

pub fn parse_pasv_response(response: &str) -> Option<(String, u16)> {
    let start = response.find('(')?;
    let end = response.rfind(')')?;
    let parts: Vec<_> = response[start + 1..end].split(',').collect();
    if parts.len() != 6 {
        return None;
    }
    let values: Option<Vec<u8>> = parts.iter().map(|part| part.trim().parse().ok()).collect();
    let values = values?;
    let [h1, h2, h3, h4, p1, p2] = values.as_slice() else {
        return None;
    };
    Some((
        format!("{h1}.{h2}.{h3}.{h4}"),
        u16::from(*p1) * 256 + u16::from(*p2),
    ))
}

pub fn parse_epsv_response(response: &str) -> Option<u16> {
    let part = if let Some(open) = response.find('(') {
        let close = response.rfind(')').filter(|close| *close > open)?;
        &response[open + 1..close]
    } else {
        response
    };
    let parts: Vec<_> = part.split('|').collect();
    if parts.len() < 5 {
        return None;
    }
    let port = parts[3].parse::<u16>().ok()?;
    (port != 0).then_some(port)
}

pub fn parse_mdtm_timestamp(value: &str) -> Option<SystemTime> {
    if value.len() < 14 {
        return None;
    }
    let year: i32 = value[0..4].parse().ok()?;
    let month: u32 = value[4..6].parse().ok()?;
    let day: u32 = value[6..8].parse().ok()?;
    let hour: u32 = value[8..10].parse().ok()?;
    let minute: u32 = value[10..12].parse().ok()?;
    let second: u32 = value[12..14].parse().ok()?;
    if !(1990..=2999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds =
        days * 86400 + u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second);
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Parse FTP SIZE data while preserving the signed file-offset limit used by
/// the download engine.
pub fn parse_size_response(response: &str) -> Result<u64, String> {
    let size = response
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("Invalid FTP SIZE response {response:?}: {error}"))?;
    if size > i64::MAX as u64 {
        return Err(format!("FTP SIZE response is too large: {size}"));
    }
    Ok(size)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<u64> {
    let year = if month <= 2 { year - 1 } else { year };
    let month = if month <= 2 { month + 9 } else { month - 3 };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400) as u64;
    let day_of_year = (153 * month as u64 + 2) / 5 + day as u64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era as u64 * 146097 + day_of_era - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_and_preserves_invalid_escapes() {
        assert_eq!(percent_decode("%E6%96%87%E4%BB%B6"), "文件");
        assert_eq!(percent_decode("%ZZ%2"), "%ZZ%2");
    }

    #[test]
    fn splits_decoded_paths_and_builds_cwd_sequence() {
        assert_eq!(
            split_decoded_remote_path("/pub/linux/file.bin"),
            ("/pub/linux".into(), "file.bin".into())
        );
        assert_eq!(
            cwd_targets("/srv", "/pub//linux"),
            vec!["/srv".to_owned(), "pub".to_owned(), "linux".to_owned()]
        );
    }

    #[test]
    fn parses_passive_responses() {
        assert_eq!(
            parse_pasv_response("227 Entering Passive Mode (192,168,1,2,195,80)"),
            Some(("192.168.1.2".into(), 50000))
        );
        assert_eq!(parse_pasv_response("227 (1,2,3)"), None);
        assert_eq!(
            parse_epsv_response("229 Entering Extended Passive Mode (|||50001|)"),
            Some(50001)
        );
        assert_eq!(parse_epsv_response("|||0|"), None);
    }

    #[test]
    fn parses_pwd_and_mdtm_responses() {
        assert_eq!(
            parse_pwd_response("257 \"/pub\" is current"),
            Some("/pub".into())
        );
        assert_eq!(parse_pwd_response("257 /pub"), None);
        assert!(parse_mdtm_timestamp("20240115103000").is_some());
        assert!(parse_mdtm_timestamp("20241315103000").is_none());
    }
}
