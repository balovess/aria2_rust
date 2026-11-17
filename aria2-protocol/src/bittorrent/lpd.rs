//! BitTorrent Local Peer Discovery (BEP 14) announcement parsing.

use std::net::SocketAddr;

use tracing::debug;

/// Parsed LPD announcement fields, independent of the receiving socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpdAnnouncement {
    pub info_hash: String,
    pub port: u16,
}

/// Encode a BEP 14 announcement for the given multicast endpoint.
pub fn encode_announcement(
    multicast_addr: SocketAddr,
    listen_port: u16,
    info_hash: &str,
) -> Result<Vec<u8>, String> {
    if listen_port == 0 {
        return Err("LPD announce requires a non-zero BitTorrent listen port".to_string());
    }
    if info_hash.len() != 40
        || !info_hash
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "Invalid info_hash format: expected 40 hex chars, got {} chars",
            info_hash.len()
        ));
    }

    Ok(format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: {}:{}\r\nPort: {}\r\nInfohash: {}\r\n\r\n\r\n",
        multicast_addr.ip(),
        multicast_addr.port(),
        listen_port,
        info_hash
    )
    .into_bytes())
}

/// Parse a BEP 14 announcement, accepting the historical Rust wire format too.
pub fn parse_announcement(data: &[u8]) -> Option<LpdAnnouncement> {
    let text = std::str::from_utf8(data).ok()?;
    let first_line = text.lines().next()?;
    if !first_line.starts_with("BT-SEARCH ") {
        return parse_legacy_announcement(data);
    }

    let mut info_hash = String::new();
    let mut port = 0u16;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("Infohash:") {
            let value = rest.trim();
            if value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit()) {
                info_hash = value.to_lowercase();
            } else {
                debug!(infohash = %value, "LPD: invalid infohash format");
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("Port:") {
            let value = rest.trim();
            match value.parse::<u16>() {
                Ok(parsed) if parsed > 0 => port = parsed,
                _ => {
                    debug!(port = %value, "LPD: invalid port");
                    return None;
                }
            }
        }
    }

    if info_hash.is_empty() || port == 0 {
        debug!(
            has_hash = !info_hash.is_empty(),
            has_port = port > 0,
            "Incomplete BEP14 LPD announcement ignored"
        );
        return None;
    }

    Some(LpdAnnouncement { info_hash, port })
}

fn parse_legacy_announcement(data: &[u8]) -> Option<LpdAnnouncement> {
    let text = std::str::from_utf8(data).ok()?;
    let mut info_hash = String::new();
    let mut port = 0u16;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Hash:") {
            let value = rest.trim();
            if value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit()) {
                info_hash = value.to_lowercase();
            } else {
                return None;
            }
        } else if let Some(rest) = line.strip_prefix("Port:") {
            port = rest.trim().parse().ok()?;
            if port == 0 {
                return None;
            }
        }
    }

    (!info_hash.is_empty() && port > 0).then_some(LpdAnnouncement { info_hash, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bep14_announcement_and_normalizes_hash() {
        let announcement = parse_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: ABCDEF0123456789ABCDEF0123456789ABCDEF01\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            announcement.info_hash,
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert_eq!(announcement.port, 6881);
    }

    #[test]
    fn accepts_the_legacy_announcement_format() {
        let announcement =
            parse_announcement(b"Hash: ABCDEF0123456789ABCDEF0123456789ABCDEF01\nPort: 6881\n")
                .unwrap();
        assert_eq!(
            announcement.info_hash,
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert_eq!(announcement.port, 6881);
    }

    #[test]
    fn rejects_invalid_announcement_fields() {
        assert!(parse_announcement(b"BT-SEARCH * HTTP/1.1\r\nPort: 0\r\n").is_none());
        assert!(
            parse_announcement(b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: bad\r\n")
                .is_none()
        );
        assert!(parse_announcement(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn encodes_the_bep14_wire_format() {
        let message = encode_announcement(
            "239.192.152.143:6771".parse().unwrap(),
            6881,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap();
        assert_eq!(
            message,
            b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n"
        );
    }
}
