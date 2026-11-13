use std::fmt;

pub const INITIAL_CONNECTION_ID: u64 = 0x41727101980;
pub const DEFAULT_ANNOUNCE_INTERVAL: u32 = 300;
/// Maximum age at which a client may reuse a UDP tracker connection ID.
pub const CONNECTION_ID_TTL_SECS: u64 = 60;

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

/// Common fields at the start of every UDP tracker response frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpTrackerResponseHeader {
    /// Raw action value, retained so callers can reject or report unknown actions.
    pub action: i32,
    pub transaction_id: u32,
}

/// Decode the action and transaction ID shared by UDP tracker responses.
///
/// Unknown action values are returned unchanged so the engine can correlate
/// the response before deciding how to handle the action.
pub fn decode_response_header(data: &[u8]) -> Option<UdpTrackerResponseHeader> {
    let header = data.get(..8)?;
    Some(UdpTrackerResponseHeader {
        action: i32::from_be_bytes(header[..4].try_into().ok()?),
        transaction_id: u32::from_be_bytes(header[4..8].try_into().ok()?),
    })
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

pub fn build_connect_request(txn_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&INITIAL_CONNECTION_ID.to_be_bytes());
    buf.extend_from_slice(&(UdpAction::Connect as i32).to_be_bytes());
    buf.extend_from_slice(&txn_id.to_be_bytes());
    assert_eq!(buf.len(), 16, "CONNECT request must be exactly 16 bytes");
    buf
}

#[allow(clippy::too_many_arguments)]
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
    buf.extend_from_slice(&left.to_be_bytes());
    buf.extend_from_slice(&uploaded.to_be_bytes());
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
    if data.len() != 16 {
        return Err(format!(
            "CONNECT response must be exactly 16 bytes, got {}",
            data.len()
        ));
    }
    let header = decode_response_header(data)
        .ok_or_else(|| "CONNECT response too short for header".to_string())?;
    if header.action != UdpAction::Connect as i32 {
        return Err(format!(
            "Unexpected action in CONNECT response: {}",
            header.action
        ));
    }
    let conn_id = u64::from_be_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    Ok(ConnectResponse {
        transaction_id: header.transaction_id,
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

pub fn parse_announce_response(data: &[u8]) -> Result<AnnounceResponse, String> {
    if data.len() < 8 {
        return Err(format!(
            "ANNOUNCE response too short: {} bytes (min 8)",
            data.len()
        ));
    }
    let header = decode_response_header(data)
        .ok_or_else(|| format!("ANNOUNCE response too short: {} bytes (min 8)", data.len()))?;
    if header.action == UdpAction::Error as i32 {
        let msg_len = (data.len() - 8).min(256);
        let msg = String::from_utf8_lossy(&data[8..8 + msg_len]);
        return Err(format!("Tracker error: {}", msg));
    }
    if header.action != UdpAction::Announce as i32 {
        return Err(format!(
            "Unexpected action in ANNOUNCE response: {}",
            header.action
        ));
    }
    if data.len() < 20 {
        return Err(format!(
            "ANNOUNCE response too short: {} bytes (min 20)",
            data.len()
        ));
    }
    if !(data.len() - 20).is_multiple_of(6) {
        return Err(format!(
            "ANNOUNCE compact peer list length {} is not a multiple of 6",
            data.len() - 20
        ));
    }
    let interval = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let leechers = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    let seeders = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let peers = if data.len() > 20 {
        crate::bittorrent::peer::address::decode_compact_v4_peers(&data[20..])
            .expect("compact peer list length was validated")
            .into_iter()
            .map(|peer| (peer.ip().to_string(), peer.port()))
            .collect()
    } else {
        Vec::new()
    };
    Ok(AnnounceResponse {
        transaction_id: header.transaction_id,
        interval,
        leechers,
        seeders,
        peers,
    })
}

/// Result of scraping one info hash from a UDP tracker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrapeResult {
    pub seeders: u32,
    pub leechers: u32,
    pub completed: u32,
}

/// Build a UDP tracker SCRAPE request
///
/// # Arguments
/// * `conn_id` - Connection ID obtained from CONNECT response
/// * `txn_id` - Transaction ID for this request
/// * `info_hashes` - Slice of 20-byte info hashes to query (max ~74 per request)
pub fn build_scrape_request(conn_id: u64, txn_id: u32, info_hashes: &[[u8; 20]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + info_hashes.len() * 20);
    buf.extend_from_slice(&conn_id.to_be_bytes());
    buf.extend_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
    buf.extend_from_slice(&txn_id.to_be_bytes());
    for ih in info_hashes {
        buf.extend_from_slice(ih);
    }
    buf
}

/// Parse a SCRAPE response from a UDP tracker
///
/// Response format:
/// - action (4 bytes, big-endian) = 2
/// - transaction_id (4 bytes)
/// - For each info hash: seeders(4), leechers(4), completed(4)
pub fn parse_scrape_response(data: &[u8]) -> Result<Vec<ScrapeResult>, String> {
    if data.len() < 8 {
        return Err(format!(
            "SCRAPE response too short: {} bytes (min 8)",
            data.len()
        ));
    }
    let header = decode_response_header(data)
        .ok_or_else(|| format!("SCRAPE response too short: {} bytes (min 8)", data.len()))?;
    if header.action != UdpAction::Scrape as i32 {
        return Err(format!(
            "Unexpected action in SCRAPE response: {}",
            header.action
        ));
    }

    // Skip action + transaction_id = 8 bytes
    let body = &data[8..];
    if !body.len().is_multiple_of(12) {
        return Err(format!(
            "SCRAPE response body length {} is not a multiple of 12",
            body.len()
        ));
    }

    let count = body.len() / 12;
    let mut results = Vec::with_capacity(count);
    for i in 0..count {
        let offset = i * 12;
        if offset + 12 > body.len() {
            return Err("SCRAPE response body truncated".into());
        }
        results.push(ScrapeResult {
            seeders: u32::from_be_bytes(
                body[offset..offset + 4]
                    .try_into()
                    .map_err(|e| format!("Failed to read seeders: {}", e))?,
            ),
            leechers: u32::from_be_bytes(
                body[offset + 4..offset + 8]
                    .try_into()
                    .map_err(|e| format!("Failed to read leechers: {}", e))?,
            ),
            completed: u32::from_be_bytes(
                body[offset + 8..offset + 12]
                    .try_into()
                    .map_err(|e| format!("Failed to read completed: {}", e))?,
            ),
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests;
