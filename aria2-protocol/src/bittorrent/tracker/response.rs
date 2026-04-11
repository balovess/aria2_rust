use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackerEvent {
    Started,
    Completed,
    Stopped,
}

impl TrackerEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerEvent::Started => "started",
            TrackerEvent::Completed => "completed",
            TrackerEvent::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackerResponse {
    pub interval: u32,
    pub min_interval: Option<u32>,
    pub seeders: u32,
    pub leechers: u32,
    pub peers: Vec<PeerInfo>,
    pub warning_message: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub ip: String,
    pub port: u16,
    pub peer_id: Option<[u8; 20]>,
}

impl TrackerResponse {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        use crate::bittorrent::bencode::codec::BencodeValue;
        let (root, _) = BencodeValue::decode(data)?;

        let failure_reason = root.dict_get_str("failure reason").map(|s| s.to_string());

        if failure_reason.is_some() && root.dict_get(b"interval").is_none() {
            return Ok(Self {
                interval: 300,
                min_interval: None,
                seeders: 0,
                leechers: 0,
                peers: vec![],
                warning_message: None,
                failure_reason,
            });
        }

        let interval = root.dict_get_int("interval").unwrap_or(1800) as u32;
        let min_interval = root.dict_get_int("min interval").map(|n| n as u32);
        let seeders = root.dict_get_int("complete").unwrap_or(0) as u32;
        let leechers = root.dict_get_int("incomplete").unwrap_or(0) as u32;
        let warning_message = root.dict_get_str("warning message").map(|s| s.to_string());

        let peers = Self::parse_peers(&root)?;

        debug!(
            "Tracker响应: interval={}s, seeders={}, leechers={}, peers={}",
            interval,
            seeders,
            leechers,
            peers.len()
        );

        Ok(Self {
            interval,
            min_interval,
            seeders,
            leechers,
            peers,
            warning_message,
            failure_reason: None,
        })
    }

    fn parse_peers(
        root: &crate::bittorrent::bencode::codec::BencodeValue,
    ) -> Result<Vec<PeerInfo>, String> {
        match root.dict_get(b"peers") {
            Some(crate::bittorrent::bencode::codec::BencodeValue::Bytes(data)) => {
                Self::parse_compact_peers(data)
            }
            Some(crate::bittorrent::bencode::codec::BencodeValue::List(list)) => {
                Self::parse_normal_peers(list)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn parse_compact_peers(data: &[u8]) -> Result<Vec<PeerInfo>, String> {
        if !data.len().is_multiple_of(6) {
            return Err(format!("compact peers数据长度({})不是6的倍数", data.len()));
        }

        let mut peers = Vec::new();
        for chunk in data.chunks_exact(6) {
            let ip = format!("{}.{}.{}.{}", chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            peers.push(PeerInfo {
                ip,
                port,
                peer_id: None,
            });
        }
        Ok(peers)
    }

    fn parse_normal_peers(
        list: &[crate::bittorrent::bencode::codec::BencodeValue],
    ) -> Result<Vec<PeerInfo>, String> {
        let mut peers = Vec::new();
        for item in list {
            let dict = item.as_dict().ok_or("peer条目不是字典")?;
            let ip = dict
                .get(&b"ip"[..])
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let port = dict
                .get(&b"port"[..])
                .and_then(|v| v.as_int())
                .map(|n| n as u16)
                .unwrap_or(0);
            let peer_id = dict
                .get(&b"peer id"[..])
                .and_then(|v| v.as_bytes())
                .filter(|b| b.len() == 20)
                .map(|b| {
                    let mut id = [0u8; 20];
                    id.copy_from_slice(b);
                    id
                });

            if !ip.is_empty() && port > 0 {
                peers.push(PeerInfo { ip, port, peer_id });
            }
        }
        Ok(peers)
    }

    pub fn is_failure(&self) -> bool {
        self.failure_reason.is_some()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    #[test]
    fn test_parse_simple_response() {
        let mut peers_data = vec![0u8; 12];
        peers_data[0..4].copy_from_slice(&[127, 0, 0, 1]);
        peers_data[4..6].copy_from_slice(&6881u16.to_be_bytes());
        peers_data[6..10].copy_from_slice(&[192, 168, 1, 1]);
        peers_data[10..12].copy_from_slice(&6882u16.to_be_bytes());

        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(900));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(peers_data));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let parsed = TrackerResponse::parse(&encoded).unwrap();

        assert_eq!(parsed.interval, 900);
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].ip, "127.0.0.1");
        assert_eq!(parsed.peers[0].port, 6881);
    }

    #[test]
    fn test_parse_failure_response() {
        let mut d = BTreeMap::new();
        d.insert(
            b"failure reason".to_vec(),
            BencodeValue::Bytes(b"tracker offline".to_vec()),
        );
        let root = BencodeValue::Dict(d);
        let resp = TrackerResponse::parse(&root.encode()).unwrap();
        assert!(resp.is_failure());
        assert_eq!(resp.failure_reason.as_deref(), Some("tracker offline"));
    }

    #[test]
    fn test_event_as_str() {
        assert_eq!(TrackerEvent::Started.as_str(), "started");
        assert_eq!(TrackerEvent::Completed.as_str(), "completed");
        assert_eq!(TrackerEvent::Stopped.as_str(), "stopped");
    }
}
