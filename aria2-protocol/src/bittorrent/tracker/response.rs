use tracing::debug;

#[derive(Debug, Clone)]
pub struct TrackerResponse {
    pub interval: u32,
    pub min_interval: Option<u32>,
    /// Tracker swarm seeders, when the response included `complete`.
    pub seeders: Option<u32>,
    /// Tracker swarm leechers, when the response included `incomplete`.
    pub leechers: Option<u32>,
    /// Number of completed torrents reported by an HTTP tracker response.
    pub downloaded: Option<u64>,
    /// Peers from the "peers" key (compact or dictionary format, typically IPv4).
    pub peers: Vec<PeerInfo>,
    /// IPv6 peers from the "peers6" key (compact format, 18 bytes per peer).
    ///
    /// Matches the C++ `BtAnnounce::PEERS6` key and `extractPeer(peer6Data,
    /// AF_INET6, ...)` call in `DefaultBtAnnounce::processAnnounceResponse`.
    /// The compact format uses 16 bytes for the IPv6 address and 2 bytes for
    /// the port (big-endian), per BEP 7.
    pub peers6: Vec<PeerInfo>,
    /// Tracker ID from the tracker response ("tracker id" key in bencode).
    /// The client must echo this back as the `trackerid` parameter in
    /// subsequent announce requests, per the BitTorrent tracker protocol.
    pub tracker_id: Option<String>,
    /// Trackers supplied by a tracker response extension.
    ///
    /// `announce-list` is normally a torrent metainfo field (BEP 12), but a
    /// few tracker services return it dynamically. Keep it separate from the
    /// peer payload so the engine can append it without replacing the
    /// torrent's configured tiers.
    pub announce_list: Vec<Vec<String>>,
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
                seeders: None,
                leechers: None,
                downloaded: None,
                peers: vec![],
                peers6: vec![],
                tracker_id: None,
                announce_list: Vec::new(),
                warning_message: None,
                failure_reason,
            });
        }

        let interval = root.dict_get_int("interval").unwrap_or(1800) as u32;
        let min_interval = root.dict_get_int("min interval").map(|n| n as u32);
        let seeders = root.dict_get_int("complete").map(|n| n as u32);
        let leechers = root.dict_get_int("incomplete").map(|n| n as u32);
        let downloaded = root
            .dict_get_int("downloaded")
            .and_then(|value| u64::try_from(value).ok());
        let warning_message = root.dict_get_str("warning message").map(|s| s.to_string());
        let tracker_id = root.dict_get_str("tracker id").map(|s| s.to_string());
        let announce_list = Self::parse_announce_list(&root);

        let peers = Self::parse_peers(&root)?;

        // Parse IPv6 compact peers ("peers6" key) per BitTorrent tracker
        // protocol extension. The C++ aria2 references this as
        // BtAnnounce::PEERS6 in DefaultBtAnnounce::processAnnounceResponse.
        let peers6 = Self::parse_peers6(&root)?;

        debug!(
            "Tracker response: interval={}s, seeders={:?}, leechers={:?}, downloaded={:?}, peers={}, peers6={}, tracker_id={:?}, announce_list={:?}",
            interval,
            seeders,
            leechers,
            downloaded,
            peers.len(),
            peers6.len(),
            tracker_id,
            announce_list,
        );

        Ok(Self {
            interval,
            min_interval,
            seeders,
            leechers,
            downloaded,
            peers,
            peers6,
            tracker_id,
            announce_list,
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

    fn parse_announce_list(
        root: &crate::bittorrent::bencode::codec::BencodeValue,
    ) -> Vec<Vec<String>> {
        let Some(crate::bittorrent::bencode::codec::BencodeValue::List(tiers)) =
            root.dict_get(b"announce-list")
        else {
            return Vec::new();
        };

        tiers
            .iter()
            .filter_map(|tier| {
                let urls = tier
                    .as_list()?
                    .iter()
                    .filter_map(|url| {
                        let url = url.as_str()?.trim();
                        (!url.is_empty()).then(|| url.to_string())
                    })
                    .collect::<Vec<_>>();
                (!urls.is_empty()).then_some(urls)
            })
            .collect()
    }

    fn parse_compact_peers(data: &[u8]) -> Result<Vec<PeerInfo>, String> {
        if !data.len().is_multiple_of(6) {
            return Err(format!(
                "Compact peers data length ({}) is not a multiple of 6",
                data.len()
            ));
        }

        let mut peers = Vec::new();
        // Use as_chunks (stable since Rust 1.88) instead of chunks_exact with a
        // constant to satisfy clippy::chunks_exact_with_constant. The length is
        // already verified to be a multiple of 6 above, so the remainder is empty.
        let (chunks, _remainder) = data.as_chunks::<6>();
        for chunk in chunks {
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
            let dict = item.as_dict().ok_or("Peer entry is not a dictionary")?;
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

    /// Parse the "peers6" key from tracker response (IPv6 compact format).
    /// The compact format is 18 bytes per peer: 16-byte IPv6 address + 2-byte port.
    fn parse_peers6(
        root: &crate::bittorrent::bencode::codec::BencodeValue,
    ) -> Result<Vec<PeerInfo>, String> {
        match root.dict_get(b"peers6") {
            Some(crate::bittorrent::bencode::codec::BencodeValue::Bytes(data)) => {
                Self::parse_compact_peers_v6(data)
            }
            // The "peers6" key only supports compact format per the protocol
            // extension; dictionary/list format is not defined for IPv6.
            _ => Ok(Vec::new()),
        }
    }

    /// Decode compact IPv6 peer data (18 bytes per peer: 16-byte IP + 2-byte port).
    fn parse_compact_peers_v6(data: &[u8]) -> Result<Vec<PeerInfo>, String> {
        use crate::bittorrent::peer::connection::PeerAddr;

        if data.is_empty() {
            return Ok(Vec::new());
        }
        if !data.len().is_multiple_of(PeerAddr::COMPACT_SIZE_V6) {
            return Err(format!(
                "Compact peers6 data length ({}) is not a multiple of {}",
                data.len(),
                PeerAddr::COMPACT_SIZE_V6
            ));
        }

        let count = data.len() / PeerAddr::COMPACT_SIZE_V6;
        let mut peers = Vec::with_capacity(count);
        for i in 0..count {
            let start = i * PeerAddr::COMPACT_SIZE_V6;
            let end = start + PeerAddr::COMPACT_SIZE_V6;
            let addr = PeerAddr::from_compact_v6(&data[start..end])
                .ok_or_else(|| format!("Failed to parse IPv6 peer at index {}", i))?;
            peers.push(PeerInfo {
                ip: addr.ip,
                port: addr.port,
                peer_id: None,
            });
        }
        Ok(peers)
    }

    pub fn is_failure(&self) -> bool {
        self.failure_reason.is_some()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len() + self.peers6.len()
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
        assert_eq!(parsed.seeders, None);
        assert_eq!(parsed.leechers, None);
        assert_eq!(parsed.downloaded, None);
        assert!(parsed.announce_list.is_empty());
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
    fn test_parse_tracker_id() {
        let mut peers_data = vec![0u8; 6];
        peers_data[0..4].copy_from_slice(&[127, 0, 0, 1]);
        peers_data[4..6].copy_from_slice(&6881u16.to_be_bytes());

        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(peers_data));
        resp_dict.insert(
            b"tracker id".to_vec(),
            BencodeValue::Bytes(b"my-tracker-42".to_vec()),
        );
        resp_dict.insert(b"complete".to_vec(), BencodeValue::Int(0));
        resp_dict.insert(b"incomplete".to_vec(), BencodeValue::Int(0));
        resp_dict.insert(b"downloaded".to_vec(), BencodeValue::Int(0));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let parsed = TrackerResponse::parse(&encoded).unwrap();

        assert_eq!(parsed.tracker_id.as_deref(), Some("my-tracker-42"));
        assert_eq!(parsed.seeders, Some(0));
        assert_eq!(parsed.leechers, Some(0));
        assert_eq!(parsed.downloaded, Some(0));
    }

    #[test]
    fn test_parse_dynamic_announce_list() {
        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(
            b"peers".to_vec(),
            BencodeValue::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1]),
        );
        resp_dict.insert(
            b"announce-list".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::List(vec![BencodeValue::Bytes(
                    b"https://tracker.example/announce".to_vec(),
                )]),
                BencodeValue::List(vec![BencodeValue::Bytes(
                    b"udp://tracker.example:6969/announce".to_vec(),
                )]),
            ]),
        );

        let parsed = TrackerResponse::parse(&BencodeValue::Dict(resp_dict).encode()).unwrap();
        assert_eq!(
            parsed.announce_list,
            vec![
                vec!["https://tracker.example/announce".to_string()],
                vec!["udp://tracker.example:6969/announce".to_string()],
            ]
        );
    }

    #[test]
    fn test_parse_no_tracker_id() {
        let mut peers_data = vec![0u8; 6];
        peers_data[0..4].copy_from_slice(&[127, 0, 0, 1]);
        peers_data[4..6].copy_from_slice(&6881u16.to_be_bytes());

        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(peers_data));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let parsed = TrackerResponse::parse(&encoded).unwrap();

        assert!(parsed.tracker_id.is_none());
    }

    #[test]
    fn test_parse_peers6_compact() {
        // Build a tracker response with both "peers" (1 IPv4 peer) and "peers6"
        // (2 IPv6 peers in compact format, 18 bytes each).
        let mut peers_data = vec![0u8; 6];
        peers_data[0..4].copy_from_slice(&[127, 0, 0, 1]);
        peers_data[4..6].copy_from_slice(&6881u16.to_be_bytes());

        // IPv6 peer 1: 2001:db8::1 port 6881
        let mut peers6_data = Vec::with_capacity(36);
        let ipv6_1 = "2001:db8::1".parse::<std::net::Ipv6Addr>().unwrap();
        peers6_data.extend_from_slice(&ipv6_1.octets());
        peers6_data.extend_from_slice(&6881u16.to_be_bytes());

        // IPv6 peer 2: ::1 port 6882
        let ipv6_2 = "::1".parse::<std::net::Ipv6Addr>().unwrap();
        peers6_data.extend_from_slice(&ipv6_2.octets());
        peers6_data.extend_from_slice(&6882u16.to_be_bytes());

        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(peers_data));
        resp_dict.insert(b"peers6".to_vec(), BencodeValue::Bytes(peers6_data));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let parsed = TrackerResponse::parse(&encoded).unwrap();

        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.peers[0].ip, "127.0.0.1");
        assert_eq!(parsed.peers[0].port, 6881);

        assert_eq!(parsed.peers6.len(), 2);
        assert_eq!(parsed.peers6[0].ip, "2001:db8::1");
        assert_eq!(parsed.peers6[0].port, 6881);
        assert_eq!(parsed.peers6[1].ip, "::1");
        assert_eq!(parsed.peers6[1].port, 6882);

        assert_eq!(parsed.peer_count(), 3);
    }

    #[test]
    fn test_parse_peers6_invalid_length() {
        // 17 bytes is not a multiple of 18 -> error
        let peers6_data = vec![0u8; 17];

        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(vec![0u8; 6]));
        resp_dict.insert(b"peers6".to_vec(), BencodeValue::Bytes(peers6_data));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let result = TrackerResponse::parse(&encoded);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("not a multiple of 18"),
            "Expected error about peers6 length not being a multiple of 18"
        );
    }

    #[test]
    fn test_parse_peers6_empty() {
        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"interval".to_vec(), BencodeValue::Int(300));
        resp_dict.insert(b"peers".to_vec(), BencodeValue::Bytes(vec![0u8; 6]));

        let root = BencodeValue::Dict(resp_dict);
        let encoded = root.encode();
        let parsed = TrackerResponse::parse(&encoded).unwrap();

        assert!(parsed.peers6.is_empty());
        assert_eq!(parsed.peer_count(), 1);
    }

    #[test]
    fn test_parse_failure_response_has_empty_peers6() {
        let mut d = BTreeMap::new();
        d.insert(
            b"failure reason".to_vec(),
            BencodeValue::Bytes(b"tracker offline".to_vec()),
        );
        let root = BencodeValue::Dict(d);
        let resp = TrackerResponse::parse(&root.encode()).unwrap();
        assert!(resp.peers.is_empty());
        assert!(resp.peers6.is_empty());
    }
}
