use std::fmt;

pub const INITIAL_CONNECTION_ID: u64 = 0x41727101980;
pub const DEFAULT_ANNOUNCE_INTERVAL: u32 = 300;
pub const CONNECTION_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum UdpAction {
    Connect = 0,
    Announce = 1,
    Scrape = 2,
    Error = 3,
}

impl UdpAction {
    pub fn from_i32(v: i32) -> Option<UdpAction> {
        match v {
            0 => Some(UdpAction::Connect),
            1 => Some(UdpAction::Announce),
            2 => Some(UdpAction::Scrape),
            3 => Some(UdpAction::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum UdpEvent {
    None = 0,
    Completed = 1,
    Started = 2,
    Stopped = 3,
}

impl fmt::Display for UdpEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UdpEvent::None => write!(f, "none"),
            UdpEvent::Completed => write!(f, "completed"),
            UdpEvent::Started => write!(f, "started"),
            UdpEvent::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpState {
    Pending,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpError {
    Success,
    TrackerError,
    Timeout,
    Network,
    Shutdown,
}

impl fmt::Display for UdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UdpError::Success => write!(f, "success"),
            UdpError::TrackerError => write!(f, "tracker_error"),
            UdpError::Timeout => write!(f, "timeout"),
            UdpError::Network => write!(f, "network"),
            UdpError::Shutdown => write!(f, "shutdown"),
        }
    }
}

pub fn build_connect_request(txn_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&INITIAL_CONNECTION_ID.to_be_bytes());
    buf.extend_from_slice(&(UdpAction::Connect as i32).to_be_bytes());
    buf.extend_from_slice(&txn_id.to_be_bytes());
    assert_eq!(buf.len(), 16, "CONNECT request must be exactly 16 bytes");
    buf
}

pub fn build_announce_request(
    conn_id: u64,
    txn_id: u32,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: i64,
    left: i64,
    uploaded: i64,
    event: UdpEvent,
    ip: u32,
    key: u32,
    num_want: i32,
    port: u16,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(100);
    buf.extend_from_slice(&conn_id.to_be_bytes());
    buf.extend_from_slice(&(UdpAction::Announce as i32).to_be_bytes());
    buf.extend_from_slice(&txn_id.to_be_bytes());
    buf.extend_from_slice(info_hash);
    buf.extend_from_slice(peer_id);
    buf.extend_from_slice(&downloaded.to_be_bytes());
    buf.extend_from_slice(&uploaded.to_be_bytes());
    buf.extend_from_slice(&left.to_be_bytes());
    buf.extend_from_slice(&(event as i32).to_be_bytes());
    buf.extend_from_slice(&ip.to_be_bytes());
    buf.extend_from_slice(&key.to_be_bytes());
    buf.extend_from_slice(&num_want.to_be_bytes());
    buf.extend_from_slice(&port.to_be_bytes());
    buf
}

pub struct ConnectResponse {
    pub transaction_id: u32,
    pub connection_id: u64,
}

pub fn parse_connect_response(data: &[u8]) -> Result<ConnectResponse, String> {
    if data.len() < 16 {
        return Err(format!(
            "CONNECT response too short: {} bytes (min 16)",
            data.len()
        ));
    }
    let action = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if action != UdpAction::Connect as i32 {
        return Err(format!("Unexpected action in CONNECT response: {}", action));
    }
    let txn_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let conn_id = u64::from_be_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    Ok(ConnectResponse {
        transaction_id: txn_id,
        connection_id: conn_id,
    })
}

#[derive(Clone)]
pub struct AnnounceResponse {
    pub transaction_id: u32,
    pub interval: u32,
    pub leechers: u32,
    pub seeders: u32,
    pub peers: Vec<(String, u16)>,
}

impl fmt::Debug for AnnounceResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnnounceResponse")
            .field("transaction_id", &self.transaction_id)
            .field("interval", &self.interval)
            .field("leechers", &self.leechers)
            .field("seeders", &self.seeders)
            .field("peers", &self.peers)
            .finish()
    }
}

fn parse_compact_peers(data: &[u8]) -> Vec<(String, u16)> {
    if data.is_empty() || !data.len().is_multiple_of(6) {
        return Vec::new();
    }
    let mut peers = Vec::with_capacity(data.len() / 6);
    for chunk in data.chunks_exact(6) {
        let ip = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        let ip_str = format!(
            "{}.{}.{}.{}",
            (ip >> 24) & 0xFF,
            (ip >> 16) & 0xFF,
            (ip >> 8) & 0xFF,
            ip & 0xFF
        );
        peers.push((ip_str, port));
    }
    peers
}

pub fn parse_announce_response(data: &[u8]) -> Result<AnnounceResponse, String> {
    if data.len() < 20 {
        return Err(format!(
            "ANNOUNCE response too short: {} bytes (min 20)",
            data.len()
        ));
    }
    let action = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    match UdpAction::from_i32(action) {
        None | Some(UdpAction::Error) => {
            let msg_len = (data.len() - 8).min(256);
            let msg = String::from_utf8_lossy(&data[8..8 + msg_len]);
            return Err(format!("Tracker error: {}", msg));
        }
        _ => {}
    }
    let txn_id = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let interval = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let leechers = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    let seeders = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let peers = if data.len() > 20 {
        parse_compact_peers(&data[20..])
    } else {
        Vec::new()
    };
    Ok(AnnounceResponse {
        transaction_id: txn_id,
        interval,
        leechers,
        seeders,
        peers,
    })
}

#[cfg(test)]
fn random_txn_id() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_nanos() & 0xFFFFFFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_connect_request_length_and_magic() {
        let req = build_connect_request(0x12345678);
        assert_eq!(req.len(), 16);

        let conn_id = u64::from_be_bytes([
            req[0], req[1], req[2], req[3], req[4], req[5], req[6], req[7],
        ]);
        assert_eq!(conn_id, INITIAL_CONNECTION_ID);

        let action = i32::from_be_bytes([req[8], req[9], req[10], req[11]]);
        assert_eq!(action, 0);

        let txn = u32::from_be_bytes([req[12], req[13], req[14], req[15]]);
        assert_eq!(txn, 0x12345678);
    }

    #[test]
    fn test_build_announce_request_format() {
        let info_hash = [0xABu8; 20];
        let peer_id = [0xCDu8; 20];
        let req = build_announce_request(
            0x123456789ABCDEF0,
            0xDEADBEEF,
            &info_hash,
            &peer_id,
            1024,
            2048,
            4096,
            UdpEvent::Started,
            0,
            0x12345678,
            50,
            6881,
        );
        assert_eq!(req.len(), 98);

        let conn_id = u64::from_be_bytes(req[..8].try_into().unwrap());
        assert_eq!(conn_id, 0x123456789ABCDEF0);

        let event = i32::from_be_bytes(req[80..84].try_into().unwrap());
        assert_eq!(event, UdpEvent::Started as i32);

        let port = u16::from_be_bytes(req[96..98].try_into().unwrap());
        assert_eq!(port, 6881);
    }

    #[test]
    fn test_parse_connect_response_valid() {
        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&(0i32).to_be_bytes()); // action=connect
        data[4..8].copy_from_slice(&0xABCDEF01u32.to_be_bytes()); // txn_id
        data[8..16].copy_from_slice(&0x123456789ABCDEF0u64.to_be_bytes()); // connection_id

        let resp = parse_connect_response(&data).unwrap();
        assert_eq!(resp.transaction_id, 0xABCDEF01);
        assert_eq!(resp.connection_id, 0x123456789ABCDEF0);
    }

    #[test]
    fn test_parse_connect_response_too_short() {
        assert!(parse_connect_response(&[0u8; 15]).is_err());
        assert!(parse_connect_response(&[]).is_err());
    }

    #[test]
    fn test_parse_connect_response_wrong_action() {
        let mut data = vec![0u8; 16];
        data[0..4].copy_from_slice(&3i32.to_be_bytes()); // action=error
        assert!(parse_connect_response(&data).is_err());
    }

    #[test]
    fn test_parse_announce_with_peers() {
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&1i32.to_be_bytes()); // action=announce
        data[4..8].copy_from_slice(&0x12345678u32.to_be_bytes()); // txn_id
        data[8..12].copy_from_slice(&900u32.to_be_bytes()); // interval=900s
        data[12..16].copy_from_slice(&5u32.to_be_bytes()); // leechers
        data[16..20].copy_from_slice(&3u32.to_be_bytes()); // seeders

        data.extend_from_slice(&[
            192, 168, 1, 1, 0x19, 0xFA, // 192.168.1.1:6650
            10, 0, 0, 1, 0x17, 0x70, // 10.0.0.1:6000
        ]);

        let resp = parse_announce_response(&data).unwrap();
        assert_eq!(resp.transaction_id, 0x12345678);
        assert_eq!(resp.interval, 900);
        assert_eq!(resp.leechers, 5);
        assert_eq!(resp.seeders, 3);
        assert_eq!(resp.peers.len(), 2);
        assert_eq!(resp.peers[0], ("192.168.1.1".into(), 6650));
        assert_eq!(resp.peers[1], ("10.0.0.1".into(), 6000));
    }

    #[test]
    fn test_parse_announce_empty_peers() {
        let mut data = vec![0u8; 20];
        data[0..4].copy_from_slice(&1i32.to_be_bytes());
        data[4..8].copy_from_slice(&0u32.to_be_bytes());
        data[8..12].copy_from_slice(&1800u32.to_be_bytes());

        let resp = parse_announce_response(&data).unwrap();
        assert_eq!(resp.interval, 1800);
        assert!(resp.peers.is_empty());
    }

    #[test]
    fn test_parse_announce_too_short() {
        assert!(parse_announce_response(&[0u8; 19]).is_err());
    }

    #[test]
    fn test_parse_announce_error_action() {
        let mut data = vec![0u8; 23];
        data[0..4].copy_from_slice(&3i32.to_be_bytes()); // error
        data[8..23].copy_from_slice(b"tracker offline");

        let result = parse_announce_response(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("tracker offline"));
    }

    #[test]
    fn test_parse_compact_peer_format() {
        let data = [
            127, 0, 0, 1, 0x1F, 0x90, // 127.0.0.1:8080
            255, 255, 255, 255, 0x00, 0x50, // 255.255.255.255:80
            10, 0, 0, 1, 0xBB, 0x82, // 10.0.0.1:48002
        ];
        let peers = parse_compact_peers(&data);
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0], ("127.0.0.1".into(), 8080));
        assert_eq!(peers[1], ("255.255.255.255".into(), 80));
        assert_eq!(peers[2], ("10.0.0.1".into(), 48002));
    }

    #[test]
    fn test_event_enum_values() {
        assert_eq!(UdpEvent::None as i32, 0);
        assert_eq!(UdpEvent::Completed as i32, 1);
        assert_eq!(UdpEvent::Started as i32, 2);
        assert_eq!(UdpEvent::Stopped as i32, 3);
    }

    #[test]
    fn test_big_endian_encoding_consistency() {
        let val: u32 = 0xAABBCCDD;
        let bytes = val.to_be_bytes();
        assert_eq!(bytes, [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(u32::from_be_bytes(bytes), val);
    }

    #[test]
    fn test_random_txn_id_produces_values() {
        let id1 = random_txn_id();
        let id2 = random_txn_id();
        assert_ne!(id1, id2, "连续两次调用应产生不同 ID");
    }

    #[test]
    fn test_udp_action_roundtrip() {
        for a in &[
            UdpAction::Connect,
            UdpAction::Announce,
            UdpAction::Scrape,
            UdpAction::Error,
        ] {
            assert_eq!(UdpAction::from_i32(*a as i32), Some(*a));
        }
        assert_eq!(UdpAction::from_i32(99), None);
    }
}
