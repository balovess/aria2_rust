//! BEP 11 `ut_pex` extension message types for BitTorrent.
//!
//! Implements the wire-format encoding/decoding for `ut_pex` (peer exchange),
//! which lets peers inform each other about other connected peers in compact
//! format, for both IPv4 and IPv6 address families.
//!
//! This type operates on the *payload* portion of a `BtMessage::Extended`,
//! i.e. the bytes **after** the 1-byte `ext_id` field. The compact-peer decode
//! helpers live in the parent `extension` module.

use std::collections::BTreeMap;

use crate::bittorrent::bencode::codec::BencodeValue;

use super::{COMPACT_PEER_V4_SIZE, COMPACT_PEER_V6_SIZE, decode_compact_v4, decode_compact_v6};

/// Compact IPv4 peer representation: 4 bytes IP + 2 bytes port = 6 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactPeerV4(pub [u8; 6]);

impl CompactPeerV4 {
    /// Get the IPv4 address bytes.
    pub fn ip(&self) -> &[u8; 4] {
        self.0[..4].try_into().unwrap()
    }

    /// Get the port in host byte order.
    pub fn port(&self) -> u16 {
        u16::from_be_bytes([self.0[4], self.0[5]])
    }
}

/// Compact IPv6 peer representation: 16 bytes IP + 2 bytes port = 18 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactPeerV6(pub [u8; 18]);

impl CompactPeerV6 {
    /// Get the IPv6 address bytes.
    pub fn ip(&self) -> &[u8; 16] {
        self.0[..16].try_into().unwrap()
    }

    /// Get the port in host byte order.
    pub fn port(&self) -> u16 {
        u16::from_be_bytes([self.0[16], self.0[17]])
    }
}

/// BEP 11 ut_pex extension message.
///
/// Wire format (bencoded dict):
/// ```text
/// d
///   5:added    <compact IPv4 peer bytes>
///   7:added.f  <IPv4 flag bytes>
///   7:added6   <compact IPv6 peer bytes>
///   9:added6.f <IPv6 flag bytes>
///   7:dropped  <compact IPv4 dropped peer bytes>
///   9:dropped6 <compact IPv6 dropped peer bytes>
/// e
/// ```
///
/// Flag byte bits (BEP 11):
/// - bit 0: peer uses encryption
/// - bit 1: peer is a seeder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtPexMessage {
    /// Newly connected IPv4 peers in compact format (6 bytes each).
    pub added: Vec<CompactPeerV4>,
    /// Flags for each IPv4 added peer (1 byte per peer).
    pub added_f: Vec<u8>,
    /// Newly connected IPv6 peers in compact format (18 bytes each).
    pub added6: Vec<CompactPeerV6>,
    /// Flags for each IPv6 added peer (1 byte per peer).
    pub added6_f: Vec<u8>,
    /// Disconnected IPv4 peers in compact format (6 bytes each).
    pub dropped: Vec<CompactPeerV4>,
    /// Disconnected IPv6 peers in compact format (18 bytes each).
    pub dropped6: Vec<CompactPeerV6>,
}

impl UtPexMessage {
    /// Create an empty PEX message.
    pub fn new() -> Self {
        Self {
            added: Vec::new(),
            added_f: Vec::new(),
            added6: Vec::new(),
            added6_f: Vec::new(),
            dropped: Vec::new(),
            dropped6: Vec::new(),
        }
    }

    /// Encode this message to the payload bytes (after the ext_id byte).
    ///
    /// Mirrors C++ `UTPexExtensionMessage::getPayload()` which separates
    /// IPv4 and IPv6 peers into distinct keys per BEP 11.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();

        // IPv4 added peers + flags
        if !self.added.is_empty() {
            let mut compact = Vec::with_capacity(self.added.len() * COMPACT_PEER_V4_SIZE);
            for peer in &self.added {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"added".to_vec(), BencodeValue::Bytes(compact));

            if !self.added_f.is_empty() {
                dict.insert(
                    b"added.f".to_vec(),
                    BencodeValue::Bytes(self.added_f.clone()),
                );
            }
        }

        // IPv6 added peers + flags
        if !self.added6.is_empty() {
            let mut compact = Vec::with_capacity(self.added6.len() * COMPACT_PEER_V6_SIZE);
            for peer in &self.added6 {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"added6".to_vec(), BencodeValue::Bytes(compact));

            if !self.added6_f.is_empty() {
                dict.insert(
                    b"added6.f".to_vec(),
                    BencodeValue::Bytes(self.added6_f.clone()),
                );
            }
        }

        // IPv4 dropped peers
        if !self.dropped.is_empty() {
            let mut compact = Vec::with_capacity(self.dropped.len() * COMPACT_PEER_V4_SIZE);
            for peer in &self.dropped {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"dropped".to_vec(), BencodeValue::Bytes(compact));
        }

        // IPv6 dropped peers
        if !self.dropped6.is_empty() {
            let mut compact = Vec::with_capacity(self.dropped6.len() * COMPACT_PEER_V6_SIZE);
            for peer in &self.dropped6 {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"dropped6".to_vec(), BencodeValue::Bytes(compact));
        }

        BencodeValue::Dict(dict).encode()
    }

    /// Parse a ut_pex payload (the bytes after the ext_id byte).
    ///
    /// Mirrors C++ `UTPexExtensionMessage::create()` which extracts
    /// added/dropped for both IPv4 and IPv6 address families.
    pub fn from_payload(payload: &[u8]) -> Result<Self, String> {
        let (val, _) = BencodeValue::decode(payload)
            .map_err(|e| format!("Failed to decode ut_pex payload: {}", e))?;

        let dict = val
            .as_dict()
            .ok_or("ut_pex payload is not a bencoded dict")?;

        let added = if let Some(bytes) = dict.get(b"added".as_slice()).and_then(|v| v.as_bytes()) {
            decode_compact_v4(bytes)?
        } else {
            Vec::new()
        };

        let added_f = dict
            .get(b"added.f".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .unwrap_or_default();

        let added6 = if let Some(bytes) = dict.get(b"added6".as_slice()).and_then(|v| v.as_bytes())
        {
            decode_compact_v6(bytes)?
        } else {
            Vec::new()
        };

        let added6_f = dict
            .get(b"added6.f".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .unwrap_or_default();

        let dropped =
            if let Some(bytes) = dict.get(b"dropped".as_slice()).and_then(|v| v.as_bytes()) {
                decode_compact_v4(bytes)?
            } else {
                Vec::new()
            };

        let dropped6 =
            if let Some(bytes) = dict.get(b"dropped6".as_slice()).and_then(|v| v.as_bytes()) {
                decode_compact_v6(bytes)?
            } else {
                Vec::new()
            };

        Ok(Self {
            added,
            added_f,
            added6,
            added6_f,
            dropped,
            dropped6,
        })
    }
}

impl Default for UtPexMessage {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ======================== UtPexMessage tests ========================

    #[test]
    fn test_pex_empty_message() {
        let msg = UtPexMessage::new();
        assert!(msg.added.is_empty());
        assert!(msg.added6.is_empty());
        assert!(msg.added_f.is_empty());
        assert!(msg.added6_f.is_empty());
        assert!(msg.dropped.is_empty());
        assert!(msg.dropped6.is_empty());

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert!(parsed.added.is_empty());
        assert!(parsed.added6.is_empty());
        assert!(parsed.dropped.is_empty());
        assert!(parsed.dropped6.is_empty());
    }

    #[test]
    fn test_pex_v4_peers_roundtrip() {
        let mut msg = UtPexMessage::new();
        // 192.168.1.1:6881
        let mut peer1 = [0u8; 6];
        peer1[..4].copy_from_slice(&[192, 168, 1, 1]);
        peer1[4..6].copy_from_slice(&6881u16.to_be_bytes());
        msg.added.push(CompactPeerV4(peer1));

        // 10.0.0.1:6882
        let mut peer2 = [0u8; 6];
        peer2[..4].copy_from_slice(&[10, 0, 0, 1]);
        peer2[4..6].copy_from_slice(&6882u16.to_be_bytes());
        msg.added.push(CompactPeerV4(peer2));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed.added.len(), 2);
        assert_eq!(parsed.added[0], CompactPeerV4(peer1));
        assert_eq!(parsed.added[1], CompactPeerV4(peer2));
    }

    #[test]
    fn test_pex_v6_peers_roundtrip() {
        let mut msg = UtPexMessage::new();
        // ::1 port 6881
        let mut peer1 = [0u8; 18];
        peer1[15] = 1; // ::1
        peer1[16..18].copy_from_slice(&6881u16.to_be_bytes());
        msg.added6.push(CompactPeerV6(peer1));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed.added6.len(), 1);
        assert_eq!(parsed.added6[0], CompactPeerV6(peer1));
    }

    #[test]
    fn test_pex_mixed_v4_v6_roundtrip() {
        let mut msg = UtPexMessage::new();

        // v4 peer
        let mut v4 = [0u8; 6];
        v4[..4].copy_from_slice(&[172, 16, 0, 1]);
        v4[4..6].copy_from_slice(&6883u16.to_be_bytes());
        msg.added.push(CompactPeerV4(v4));

        // v6 peer
        let mut v6 = [0u8; 18];
        v6[..16].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        v6[16..18].copy_from_slice(&6884u16.to_be_bytes());
        msg.added6.push(CompactPeerV6(v6));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed.added.len(), 1);
        assert_eq!(parsed.added6.len(), 1);
        assert_eq!(parsed.added[0].port(), 6883);
        assert_eq!(parsed.added6[0].port(), 6884);
    }

    #[test]
    fn test_pex_invalid_v4_data_length() {
        // 5 bytes is not a multiple of 6
        let mut dict = BTreeMap::new();
        dict.insert(b"added".to_vec(), BencodeValue::Bytes(vec![1, 2, 3, 4, 5]));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtPexMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("IPv4"));
    }

    #[test]
    fn test_pex_invalid_v6_data_length() {
        // 17 bytes is not a multiple of 18
        let mut dict = BTreeMap::new();
        dict.insert(b"added6".to_vec(), BencodeValue::Bytes(vec![0u8; 17]));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtPexMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("IPv6"));
    }

    #[test]
    fn test_pex_not_a_dict() {
        // List instead of dict
        let payload = BencodeValue::List(vec![]).encode();
        let result = UtPexMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a bencoded dict"));
    }

    #[test]
    fn test_compact_peer_v4_accessors() {
        let mut bytes = [0u8; 6];
        bytes[..4].copy_from_slice(&[127, 0, 0, 1]);
        bytes[4..6].copy_from_slice(&8080u16.to_be_bytes());
        let peer = CompactPeerV4(bytes);
        assert_eq!(peer.ip(), &[127, 0, 0, 1]);
        assert_eq!(peer.port(), 8080);
    }

    #[test]
    fn test_compact_peer_v6_accessors() {
        let mut bytes = [0u8; 18];
        bytes[15] = 1; // ::1
        bytes[16..18].copy_from_slice(&9999u16.to_be_bytes());
        let peer = CompactPeerV6(bytes);
        assert_eq!(peer.port(), 9999);
        assert_eq!(peer.ip()[15], 1);
    }

    #[test]
    fn test_pex_dropped_v4_roundtrip() {
        let mut msg = UtPexMessage::new();
        let mut peer = [0u8; 6];
        peer[..4].copy_from_slice(&[192, 168, 1, 1]);
        peer[4..6].copy_from_slice(&6881u16.to_be_bytes());
        msg.dropped.push(CompactPeerV4(peer));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert!(parsed.added.is_empty());
        assert!(parsed.added6.is_empty());
        assert_eq!(parsed.dropped.len(), 1);
        assert_eq!(parsed.dropped[0].port(), 6881);
    }

    #[test]
    fn test_pex_dropped_v6_roundtrip() {
        let mut msg = UtPexMessage::new();
        let mut peer = [0u8; 18];
        peer[15] = 1; // ::1
        peer[16..18].copy_from_slice(&6881u16.to_be_bytes());
        msg.dropped6.push(CompactPeerV6(peer));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert!(parsed.added.is_empty());
        assert!(parsed.added6.is_empty());
        assert_eq!(parsed.dropped6.len(), 1);
        assert_eq!(parsed.dropped6[0].port(), 6881);
    }

    #[test]
    fn test_pex_added_f_roundtrip() {
        let mut msg = UtPexMessage::new();
        let mut peer1 = [0u8; 6];
        peer1[..4].copy_from_slice(&[10, 0, 0, 1]);
        peer1[4..6].copy_from_slice(&6881u16.to_be_bytes());
        msg.added.push(CompactPeerV4(peer1));

        let mut peer2 = [0u8; 6];
        peer2[..4].copy_from_slice(&[10, 0, 0, 2]);
        peer2[4..6].copy_from_slice(&6882u16.to_be_bytes());
        msg.added.push(CompactPeerV4(peer2));

        // bit 0 = encryption, bit 1 = seeder
        msg.added_f = vec![0x01, 0x03];

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed.added_f.len(), 2);
        assert_eq!(parsed.added_f[0], 0x01);
        assert_eq!(parsed.added_f[1], 0x03);
    }

    #[test]
    fn test_pex_added6_f_roundtrip() {
        let mut msg = UtPexMessage::new();
        let mut peer = [0u8; 18];
        peer[..16].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        peer[16..18].copy_from_slice(&6881u16.to_be_bytes());
        msg.added6.push(CompactPeerV6(peer));

        msg.added6_f = vec![0x02]; // seeder

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed.added6_f.len(), 1);
        assert_eq!(parsed.added6_f[0], 0x02);
    }

    #[test]
    fn test_pex_full_bep11_roundtrip() {
        let mut msg = UtPexMessage::new();

        // IPv4 added
        let mut v4_added = [0u8; 6];
        v4_added[..4].copy_from_slice(&[192, 168, 1, 1]);
        v4_added[4..6].copy_from_slice(&6881u16.to_be_bytes());
        msg.added.push(CompactPeerV4(v4_added));
        msg.added_f.push(0x02);

        // IPv6 added
        let mut v6_added = [0u8; 18];
        v6_added[15] = 1;
        v6_added[16..18].copy_from_slice(&6882u16.to_be_bytes());
        msg.added6.push(CompactPeerV6(v6_added));
        msg.added6_f.push(0x01);

        // IPv4 dropped
        let mut v4_dropped = [0u8; 6];
        v4_dropped[..4].copy_from_slice(&[10, 0, 0, 1]);
        v4_dropped[4..6].copy_from_slice(&6883u16.to_be_bytes());
        msg.dropped.push(CompactPeerV4(v4_dropped));

        // IPv6 dropped
        let mut v6_dropped = [0u8; 18];
        v6_dropped[15] = 2;
        v6_dropped[16..18].copy_from_slice(&6884u16.to_be_bytes());
        msg.dropped6.push(CompactPeerV6(v6_dropped));

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();

        assert_eq!(parsed.added.len(), 1);
        assert_eq!(parsed.added_f.len(), 1);
        assert_eq!(parsed.added_f[0], 0x02);
        assert_eq!(parsed.added6.len(), 1);
        assert_eq!(parsed.added6_f.len(), 1);
        assert_eq!(parsed.added6_f[0], 0x01);
        assert_eq!(parsed.dropped.len(), 1);
        assert_eq!(parsed.dropped[0].port(), 6883);
        assert_eq!(parsed.dropped6.len(), 1);
        assert_eq!(parsed.dropped6[0].port(), 6884);
    }

    #[test]
    fn test_pex_parse_dropped6_without_added6() {
        let mut dict = BTreeMap::new();
        let mut v6_dropped = [0u8; 18];
        v6_dropped[15] = 1;
        v6_dropped[16..18].copy_from_slice(&6881u16.to_be_bytes());
        dict.insert(
            b"dropped6".to_vec(),
            BencodeValue::Bytes(v6_dropped.to_vec()),
        );

        let payload = BencodeValue::Dict(dict).encode();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();

        assert!(parsed.added.is_empty());
        assert!(parsed.added6.is_empty());
        assert!(parsed.dropped.is_empty());
        assert_eq!(parsed.dropped6.len(), 1);
    }
}
