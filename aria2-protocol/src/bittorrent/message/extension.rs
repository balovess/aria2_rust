//! BEP 10 Extension Protocol message types for BitTorrent.
//!
//! Implements the wire-format encoding/decoding for:
//! - **Extension Handshake** (BEP 10): negotiate supported extensions with a peer.
//! - **ut_metadata** (BEP 9): exchange torrent metadata pieces without a .torrent file.
//! - **ut_pex** (BEP 11): peer exchange — inform peers about other peers.
//!
//! These types operate on the *payload* portion of a `BtMessage::Extended`,
//! i.e. the bytes **after** the 1-byte `ext_id` field. The `ext_id` itself
//! is handled at the `BtMessage` layer.

use std::collections::BTreeMap;

use tracing::debug;

use crate::bittorrent::bencode::codec::BencodeValue;

// ---------------------------------------------------------------------------
// Extension Handshake (BEP 10)
// ---------------------------------------------------------------------------

/// BEP 10 extension handshake payload.
///
/// Wire format (bencoded dict):
/// ```text
/// d
///   1:m d
///     10:ut_metadata i<id>e
///     6:ut_pex      i<id>e
///   e
///   1:v            <client version string>
///   12:metadata_size i<size>e
///   1:p            i<port>e
///   4:reqq         i<max_outstanding>e
/// e
/// ```
///
/// Default values match the C++ `DefaultExtensionMessageFactory`:
/// `ut_metadata = 1`, `ut_pex = 2`, `reqq = 500`, `v = "aria2-rust/0.2"`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionHandshake {
    /// The `m` sub-dictionary mapping extension names to their negotiated ext_id.
    m_dict: BTreeMap<Vec<u8>, BencodeValue>,
    /// Client version string (BEP 10 `v` key). Defaults to "aria2-rust/0.2".
    v: Option<String>,
    /// Total metadata size in bytes for magnet links (BEP 9 `metadata_size` key).
    /// Only set when the sender has metadata to share.
    metadata_size: Option<u32>,
    /// TCP listen port (BEP 10 `p` key). Only included when > 0.
    port: Option<u16>,
    /// Maximum outstanding metadata requests the sender will accept.
    reqq: u32,
}

/// Default ext_id for ut_metadata in the `m` dict.
const DEFAULT_UT_METADATA_ID: u8 = 1;
/// Default ext_id for ut_pex in the `m` dict.
const DEFAULT_UT_PEX_ID: u8 = 2;
/// Default maximum outstanding metadata requests (reqq).
const DEFAULT_REQQ: u32 = 500;
/// Default client version string sent in the `v` key.
const DEFAULT_CLIENT_VERSION: &str = "aria2-rust/0.2";
/// Maximum metadata size accepted from peers (8 MiB), matching C++ aria2.
const MAX_METADATA_SIZE: u32 = 8 * 1024 * 1024;

impl ExtensionHandshake {
    /// Create a new handshake with default values:
    /// ut_metadata=1, ut_pex=2, reqq=500, v="aria2-rust/0.2".
    pub fn new() -> Self {
        let mut m_dict = BTreeMap::new();
        m_dict.insert(
            b"ut_metadata".to_vec(),
            BencodeValue::Int(DEFAULT_UT_METADATA_ID as i64),
        );
        m_dict.insert(
            b"ut_pex".to_vec(),
            BencodeValue::Int(DEFAULT_UT_PEX_ID as i64),
        );

        Self {
            m_dict,
            v: Some(DEFAULT_CLIENT_VERSION.to_string()),
            metadata_size: None,
            port: None,
            reqq: DEFAULT_REQQ,
        }
    }

    /// Encode this handshake to its bencoded wire representation.
    ///
    /// Matches the C++ `HandshakeExtensionMessage::getPayload()` serialization
    /// order: `v`, `p`, `m`, `metadata_size`, `reqq`. BTreeMap key ordering
    /// may differ but bencode dicts are order-independent.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut root = BTreeMap::new();

        // v — client version string (only when present)
        if let Some(ref v) = self.v {
            root.insert(b"v".to_vec(), BencodeValue::Bytes(v.as_bytes().to_vec()));
        }

        // p — TCP listen port (only when present)
        if let Some(p) = self.port {
            root.insert(b"p".to_vec(), BencodeValue::Int(p as i64));
        }

        // m sub-dict
        root.insert(b"m".to_vec(), BencodeValue::Dict(self.m_dict.clone()));

        // metadata_size (only when present)
        if let Some(ms) = self.metadata_size {
            root.insert(b"metadata_size".to_vec(), BencodeValue::Int(ms as i64));
        }

        // reqq
        root.insert(
            b"reqq".to_vec(),
            BencodeValue::Int(self.reqq as i64),
        );

        BencodeValue::Dict(root).encode()
    }

    /// Parse a bencoded extension handshake payload.
    ///
    /// Mirrors C++ `HandshakeExtensionMessage::create()` validation:
    /// - `p`: must satisfy `0 < port < 65536`
    /// - `metadata_size`: must satisfy `0 < size <= 8 MiB`
    /// - `v`: client version string (may be non-UTF-8; only valid UTF-8 accepted)
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let (val, consumed) = BencodeValue::decode(data)
            .map_err(|e| format!("Failed to decode extension handshake: {}", e))?;

        // For handshake payloads, we expect the entire input to be consumed.
        // However, some implementations include trailing data; we only warn.
        if consumed != data.len() {
            debug!(
                "Extension handshake: decoded {} bytes, total {} bytes (trailing data ignored)",
                consumed,
                data.len()
            );
        }

        let dict = val.as_dict().ok_or("Extension handshake payload is not a dict")?;

        let m_val = dict
            .get(b"m".as_slice())
            .ok_or("Missing 'm' key in extension handshake")?;

        let m_inner = m_val
            .as_dict()
            .ok_or("'m' value is not a dict in extension handshake")?;

        // Clone the m dict contents
        let m_dict = m_inner.clone();

        // Parse reqq (default to DEFAULT_REQQ if absent)
        let reqq = dict
            .get(b"reqq".as_slice())
            .and_then(|v| v.as_int())
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(DEFAULT_REQQ);

        // Parse v — client version string (optional, UTF-8 only)
        let v = dict
            .get(b"v".as_slice())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Parse p — TCP listen port (optional, must be 1..=65535)
        // Mirrors C++: `0 < port->i() && port->i() < 65536`
        let port = dict
            .get(b"p".as_slice())
            .and_then(|v| v.as_int())
            .filter(|&i| i > 0 && i <= u16::MAX as i64)
            .and_then(|i| u16::try_from(i).ok());

        // Parse metadata_size (optional, must be 1..=8 MiB)
        // Mirrors C++: `0 < size && size <= 8_m`
        let metadata_size = dict
            .get(b"metadata_size".as_slice())
            .and_then(|v| v.as_int())
            .filter(|&i| i > 0 && i <= MAX_METADATA_SIZE as i64)
            .and_then(|i| u32::try_from(i).ok());

        Ok(Self {
            m_dict,
            v,
            metadata_size,
            port,
            reqq,
        })
    }

    /// Get the negotiated ut_metadata ext_id from the `m` dict, if present.
    pub fn ut_metadata_id(&self) -> Option<u8> {
        self.m_dict
            .get(b"ut_metadata".as_slice())
            .and_then(|v| v.as_int())
            .and_then(|i| u8::try_from(i).ok())
    }

    /// Get the negotiated ut_pex ext_id from the `m` dict, if present.
    pub fn ut_pex_id(&self) -> Option<u8> {
        self.m_dict
            .get(b"ut_pex".as_slice())
            .and_then(|v| v.as_int())
            .and_then(|i| u8::try_from(i).ok())
    }

    /// Set the ut_metadata ext_id in the `m` dict. Returns `&mut Self` for chaining.
    pub fn with_ut_metadata(&mut self, id: u8) -> &mut Self {
        self.m_dict
            .insert(b"ut_metadata".to_vec(), BencodeValue::Int(id as i64));
        self
    }

    /// Set the ut_pex ext_id in the `m` dict. Returns `&mut Self` for chaining.
    pub fn with_ut_pex(&mut self, id: u8) -> &mut Self {
        self.m_dict
            .insert(b"ut_pex".to_vec(), BencodeValue::Int(id as i64));
        self
    }

    /// Get the reqq value from the handshake (max outstanding metadata requests).
    pub fn reqq(&self) -> u32 {
        self.reqq
    }

    /// Set the reqq value. Returns `&mut Self` for chaining.
    pub fn with_reqq(&mut self, reqq: u32) -> &mut Self {
        self.reqq = reqq;
        self
    }

    /// Get the client version string (`v` key), if present.
    pub fn v(&self) -> Option<&str> {
        self.v.as_deref()
    }

    /// Set the client version string. Returns `&mut Self` for chaining.
    pub fn with_version(&mut self, v: impl Into<String>) -> &mut Self {
        self.v = Some(v.into());
        self
    }

    /// Clear the client version string (omit `v` from the handshake).
    pub fn without_version(&mut self) -> &mut Self {
        self.v = None;
        self
    }

    /// Get the total metadata size in bytes (`metadata_size` key), if present.
    pub fn metadata_size(&self) -> Option<u32> {
        self.metadata_size
    }

    /// Set the total metadata size. Returns `&mut Self` for chaining.
    pub fn with_metadata_size(&mut self, size: u32) -> &mut Self {
        self.metadata_size = Some(size);
        self
    }

    /// Get the TCP listen port (`p` key), if present.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Set the TCP listen port. Returns `&mut Self` for chaining.
    pub fn with_port(&mut self, port: u16) -> &mut Self {
        self.port = Some(port);
        self
    }

    /// Access the raw `m` dictionary.
    pub fn m_dict(&self) -> &BTreeMap<Vec<u8>, BencodeValue> {
        &self.m_dict
    }
}

impl Default for ExtensionHandshake {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ut_metadata (BEP 9)
// ---------------------------------------------------------------------------

/// BEP 9 ut_metadata extension message.
///
/// The wire format for a ut_metadata payload is a bencoded dict followed
/// (for `Data` messages only) by raw metadata bytes. Specifically:
///
/// - **Request** (msg_type=0): `d8:msg_typei0e5:piecei< N >ee`
/// - **Data**    (msg_type=1): `d8:msg_typei1e5:piecei< N >10:total_sizei< S >ee<raw bytes>`
/// - **Reject**  (msg_type=2): `d8:msg_typei2e5:piecei< N >ee`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UtMetadataMessage {
    /// Request metadata piece `piece` (msg_type = 0).
    Request { piece: u32 },
    /// Deliver metadata piece `piece` of `total_size` bytes (msg_type = 1).
    /// `data` holds the raw metadata bytes appended *after* the bencoded dict.
    Data {
        piece: u32,
        total_size: u32,
        data: Vec<u8>,
    },
    /// Reject metadata piece request (msg_type = 2).
    Reject { piece: u32 },
}

impl UtMetadataMessage {
    /// Encode this message to the payload bytes (after the ext_id byte).
    ///
    /// For `Data` messages, the raw metadata bytes are appended after the
    /// bencoded dictionary, per BEP 9.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();

        match self {
            UtMetadataMessage::Request { piece } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(0));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
            UtMetadataMessage::Data {
                piece,
                total_size,
                data: _,
            } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(1));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
                dict.insert(
                    b"total_size".to_vec(),
                    BencodeValue::Int(*total_size as i64),
                );
            }
            UtMetadataMessage::Reject { piece } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(2));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
        }

        let mut payload = BencodeValue::Dict(dict).encode();

        // For Data messages, append the raw metadata bytes after the bencoded dict.
        if let UtMetadataMessage::Data { data, .. } = self {
            payload.extend_from_slice(data);
        }

        payload
    }

    /// Parse a ut_metadata payload (the bytes after the ext_id byte).
    ///
    /// For `Data` messages, the raw metadata piece is the trailing bytes
    /// after the bencoded dict.
    pub fn from_payload(payload: &[u8]) -> Result<Self, String> {
        let (val, consumed) = BencodeValue::decode(payload)
            .map_err(|e| format!("Failed to decode ut_metadata payload: {}", e))?;

        let dict = val
            .as_dict()
            .ok_or("ut_metadata payload is not a bencoded dict")?;

        let msg_type = dict
            .get(b"msg_type".as_slice())
            .and_then(|v| v.as_int())
            .ok_or("Missing 'msg_type' in ut_metadata payload")? as u32;

        let piece = dict
            .get(b"piece".as_slice())
            .and_then(|v| v.as_int())
            .ok_or("Missing 'piece' in ut_metadata payload")? as u32;

        match msg_type {
            0 => Ok(UtMetadataMessage::Request { piece }),
            1 => {
                let total_size = dict
                    .get(b"total_size".as_slice())
                    .and_then(|v| v.as_int())
                    .ok_or("Missing 'total_size' in ut_metadata Data message")? as u32;

                // The raw metadata bytes follow the bencoded dict.
                let data = payload[consumed..].to_vec();

                Ok(UtMetadataMessage::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            2 => Ok(UtMetadataMessage::Reject { piece }),
            _ => Err(format!("Unknown ut_metadata msg_type: {}", msg_type)),
        }
    }
}

// ---------------------------------------------------------------------------
// ut_pex (BEP 11)
// ---------------------------------------------------------------------------

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
///   5:added  <compact IPv4 peer bytes>
///   7:added6 <compact IPv6 peer bytes>
/// e
/// ```
///
/// Optional keys `added.f` / `added6.f` (flag bytes) are parsed but not
/// modeled here; they can be added when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtPexMessage {
    /// IPv4 peers in compact format (6 bytes each).
    pub added: Vec<CompactPeerV4>,
    /// IPv6 peers in compact format (18 bytes each).
    pub added6: Vec<CompactPeerV6>,
}

/// Compact peer size constants.
const COMPACT_PEER_V4_SIZE: usize = 6;
const COMPACT_PEER_V6_SIZE: usize = 18;

impl UtPexMessage {
    /// Create an empty PEX message.
    pub fn new() -> Self {
        Self {
            added: Vec::new(),
            added6: Vec::new(),
        }
    }

    /// Encode this message to the payload bytes (after the ext_id byte).
    pub fn to_payload(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();

        if !self.added.is_empty() {
            let mut compact = Vec::with_capacity(self.added.len() * COMPACT_PEER_V4_SIZE);
            for peer in &self.added {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"added".to_vec(), BencodeValue::Bytes(compact));
        }

        if !self.added6.is_empty() {
            let mut compact = Vec::with_capacity(self.added6.len() * COMPACT_PEER_V6_SIZE);
            for peer in &self.added6 {
                compact.extend_from_slice(&peer.0);
            }
            dict.insert(b"added6".to_vec(), BencodeValue::Bytes(compact));
        }

        BencodeValue::Dict(dict).encode()
    }

    /// Parse a ut_pex payload (the bytes after the ext_id byte).
    pub fn from_payload(payload: &[u8]) -> Result<Self, String> {
        let (val, _) = BencodeValue::decode(payload)
            .map_err(|e| format!("Failed to decode ut_pex payload: {}", e))?;

        let dict = val
            .as_dict()
            .ok_or("ut_pex payload is not a bencoded dict")?;

        let added = if let Some(bytes) = dict.get(b"added".as_slice()).and_then(|v| v.as_bytes())
        {
            decode_compact_v4(bytes)?
        } else {
            Vec::new()
        };

        let added6 =
            if let Some(bytes) = dict.get(b"added6".as_slice()).and_then(|v| v.as_bytes()) {
                decode_compact_v6(bytes)?
            } else {
                Vec::new()
            };

        Ok(Self { added, added6 })
    }
}

impl Default for UtPexMessage {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Decode compact IPv4 peer data (6 bytes per peer).
fn decode_compact_v4(data: &[u8]) -> Result<Vec<CompactPeerV4>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if !data.len().is_multiple_of(COMPACT_PEER_V4_SIZE) {
        return Err(format!(
            "Invalid compact IPv4 peer data length: {} (must be multiple of {})",
            data.len(),
            COMPACT_PEER_V4_SIZE
        ));
    }
    let count = data.len() / COMPACT_PEER_V4_SIZE;
    let mut peers = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * COMPACT_PEER_V4_SIZE;
        let arr: [u8; 6] = data[start..start + COMPACT_PEER_V4_SIZE]
            .try_into()
            .map_err(|_| "Unexpected error converting compact peer bytes".to_string())?;
        peers.push(CompactPeerV4(arr));
    }
    Ok(peers)
}

/// Decode compact IPv6 peer data (18 bytes per peer).
fn decode_compact_v6(data: &[u8]) -> Result<Vec<CompactPeerV6>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if !data.len().is_multiple_of(COMPACT_PEER_V6_SIZE) {
        return Err(format!(
            "Invalid compact IPv6 peer data length: {} (must be multiple of {})",
            data.len(),
            COMPACT_PEER_V6_SIZE
        ));
    }
    let count = data.len() / COMPACT_PEER_V6_SIZE;
    let mut peers = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * COMPACT_PEER_V6_SIZE;
        let arr: [u8; 18] = data[start..start + COMPACT_PEER_V6_SIZE]
            .try_into()
            .map_err(|_| "Unexpected error converting compact peer bytes".to_string())?;
        peers.push(CompactPeerV6(arr));
    }
    Ok(peers)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ======================== ExtensionHandshake tests ========================

    #[test]
    fn test_handshake_default_creation() {
        let hs = ExtensionHandshake::new();
        assert_eq!(hs.ut_metadata_id(), Some(1));
        assert_eq!(hs.ut_pex_id(), Some(2));
        assert_eq!(hs.reqq(), 500);
        assert_eq!(hs.v(), Some("aria2-rust/0.2"));
        assert_eq!(hs.metadata_size(), None);
        assert_eq!(hs.port(), None);
    }

    #[test]
    fn test_handshake_default_trait() {
        let hs = ExtensionHandshake::default();
        assert_eq!(hs.ut_metadata_id(), Some(1));
        assert_eq!(hs.ut_pex_id(), Some(2));
        assert_eq!(hs.v(), Some("aria2-rust/0.2"));
    }

    #[test]
    fn test_handshake_serialization_roundtrip() {
        let hs = ExtensionHandshake::new();
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ut_metadata_id(), Some(1));
        assert_eq!(parsed.ut_pex_id(), Some(2));
        assert_eq!(parsed.v(), Some("aria2-rust/0.2"));
        assert_eq!(parsed.metadata_size(), None);
        assert_eq!(parsed.port(), None);
    }

    #[test]
    fn test_handshake_with_custom_ext_ids() {
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(5).with_ut_pex(7);
        assert_eq!(hs.ut_metadata_id(), Some(5));
        assert_eq!(hs.ut_pex_id(), Some(7));

        // Roundtrip
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ut_metadata_id(), Some(5));
        assert_eq!(parsed.ut_pex_id(), Some(7));
    }

    #[test]
    fn test_handshake_missing_m_key() {
        // A dict without 'm' key should fail
        let mut dict = BTreeMap::new();
        dict.insert(b"reqq".to_vec(), BencodeValue::Int(500));
        let bytes = BencodeValue::Dict(dict).encode();
        let result = ExtensionHandshake::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing 'm' key"));
    }

    #[test]
    fn test_handshake_m_not_dict() {
        // 'm' value is not a dict
        let mut dict = BTreeMap::new();
        dict.insert(b"m".to_vec(), BencodeValue::Int(42));
        let bytes = BencodeValue::Dict(dict).encode();
        let result = ExtensionHandshake::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a dict"));
    }

    #[test]
    fn test_handshake_missing_ut_metadata() {
        // m dict without ut_metadata — ut_metadata_id returns None
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(3));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert!(parsed.ut_metadata_id().is_none());
        assert_eq!(parsed.ut_pex_id(), Some(3));
    }

    #[test]
    fn test_handshake_missing_ut_pex() {
        // m dict without ut_pex — ut_pex_id returns None
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ut_metadata_id(), Some(1));
        assert!(parsed.ut_pex_id().is_none());
    }

    #[test]
    fn test_handshake_invalid_bencode() {
        let result = ExtensionHandshake::from_bytes(b"not bencoded");
        assert!(result.is_err());
    }

    #[test]
    fn test_handshake_empty_input() {
        let result = ExtensionHandshake::from_bytes(b"");
        assert!(result.is_err());
    }

    // ======================== UtMetadataMessage tests ========================

    #[test]
    fn test_ut_metadata_request_roundtrip() {
        let msg = UtMetadataMessage::Request { piece: 0 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_request_nonzero_piece() {
        let msg = UtMetadataMessage::Request { piece: 42 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, UtMetadataMessage::Request { piece: 42 });
    }

    #[test]
    fn test_ut_metadata_data_roundtrip() {
        let metadata_piece = b"fake torrent metadata bytes".to_vec();
        let msg = UtMetadataMessage::Data {
            piece: 3,
            total_size: 50000,
            data: metadata_piece,
        };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_data_empty_piece() {
        let msg = UtMetadataMessage::Data {
            piece: 0,
            total_size: 0,
            data: Vec::new(),
        };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_reject_roundtrip() {
        let msg = UtMetadataMessage::Reject { piece: 7 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_missing_msg_type() {
        // Dict without msg_type
        let mut dict = BTreeMap::new();
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("msg_type"));
    }

    #[test]
    fn test_ut_metadata_missing_piece() {
        // Dict without piece
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("piece"));
    }

    #[test]
    fn test_ut_metadata_data_missing_total_size() {
        // Data message without total_size
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(1));
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("total_size"));
    }

    #[test]
    fn test_ut_metadata_unknown_msg_type() {
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(99));
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown"));
    }

    #[test]
    fn test_ut_metadata_invalid_bencode() {
        let result = UtMetadataMessage::from_payload(b"garbage");
        assert!(result.is_err());
    }

    // ======================== UtPexMessage tests ========================

    #[test]
    fn test_pex_empty_message() {
        let msg = UtPexMessage::new();
        assert!(msg.added.is_empty());
        assert!(msg.added6.is_empty());

        let payload = msg.to_payload();
        let parsed = UtPexMessage::from_payload(&payload).unwrap();
        assert!(parsed.added.is_empty());
        assert!(parsed.added6.is_empty());
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
        dict.insert(
            b"added".to_vec(),
            BencodeValue::Bytes(vec![1, 2, 3, 4, 5]),
        );
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtPexMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("IPv4"));
    }

    #[test]
    fn test_pex_invalid_v6_data_length() {
        // 17 bytes is not a multiple of 18
        let mut dict = BTreeMap::new();
        dict.insert(
            b"added6".to_vec(),
            BencodeValue::Bytes(vec![0u8; 17]),
        );
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
    fn test_handshake_bencode_wire_format() {
        // Verify the actual bytes produced match expected bencode structure
        let hs = ExtensionHandshake::new();
        let bytes = hs.to_bytes();

        // Must start with 'd' and end with 'e'
        assert_eq!(bytes[0], b'd');
        assert_eq!(bytes[bytes.len() - 1], b'e');

        // Must contain "ut_metadata" and "ut_pex" keys
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("ut_metadata"));
        assert!(s.contains("ut_pex"));
        assert!(s.contains("reqq"));
        // Default handshake includes client version string
        assert!(s.contains("aria2-rust/0.2"));
        // No metadata_size or port in default handshake
        assert!(!s.contains("metadata_size"));
    }

    #[test]
    fn test_handshake_reqq_parsed_from_peer() {
        // A peer sends reqq=1000
        let mut hs = ExtensionHandshake::new();
        hs.with_reqq(1000);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.reqq(), 1000);
    }

    #[test]
    fn test_handshake_reqq_default_when_missing() {
        // A peer sends handshake without reqq — we should default to 500
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(2));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        // No reqq key
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.reqq(), 500);
    }

    // ======================== New field tests (v, metadata_size, port) ========================

    #[test]
    fn test_handshake_version_roundtrip() {
        let mut hs = ExtensionHandshake::new();
        hs.with_version("aria2/1.37.0");
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.v(), Some("aria2/1.37.0"));
    }

    #[test]
    fn test_handshake_version_default() {
        let hs = ExtensionHandshake::new();
        assert_eq!(hs.v(), Some("aria2-rust/0.2"));
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.v(), Some("aria2-rust/0.2"));
    }

    #[test]
    fn test_handshake_without_version() {
        let mut hs = ExtensionHandshake::new();
        hs.without_version();
        assert_eq!(hs.v(), None);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.v(), None);
    }

    #[test]
    fn test_handshake_metadata_size_roundtrip() {
        let mut hs = ExtensionHandshake::new();
        hs.with_metadata_size(12345);
        assert_eq!(hs.metadata_size(), Some(12345));
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), Some(12345));
    }

    #[test]
    fn test_handshake_metadata_size_not_present() {
        let hs = ExtensionHandshake::new();
        assert_eq!(hs.metadata_size(), None);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), None);
    }

    #[test]
    fn test_handshake_metadata_size_zero_rejected() {
        // metadata_size=0 should be treated as absent (C++ checks size > 0)
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"metadata_size".to_vec(), BencodeValue::Int(0));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), None);
    }

    #[test]
    fn test_handshake_metadata_size_negative_rejected() {
        // Negative metadata_size should be rejected (C++ throws on negative)
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"metadata_size".to_vec(), BencodeValue::Int(-1));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), None);
    }

    #[test]
    fn test_handshake_metadata_size_max_8mib() {
        // 8 MiB is the maximum accepted metadata_size (C++ aria2 limit)
        let max_size = 8 * 1024 * 1024; // 8 MiB
        let mut hs = ExtensionHandshake::new();
        hs.with_metadata_size(max_size);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), Some(max_size));
    }

    #[test]
    fn test_handshake_metadata_size_over_8mib_rejected() {
        // metadata_size > 8 MiB should be rejected
        let oversized = 8 * 1024 * 1024 + 1;
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"metadata_size".to_vec(), BencodeValue::Int(oversized as i64));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.metadata_size(), None);
    }

    #[test]
    fn test_handshake_port_roundtrip() {
        let mut hs = ExtensionHandshake::new();
        hs.with_port(6881);
        assert_eq!(hs.port(), Some(6881));
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.port(), Some(6881));
    }

    #[test]
    fn test_handshake_port_not_present() {
        let hs = ExtensionHandshake::new();
        assert_eq!(hs.port(), None);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.port(), None);
    }

    #[test]
    fn test_handshake_port_zero_rejected() {
        // port=0 should be treated as absent (C++ checks port > 0)
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"p".to_vec(), BencodeValue::Int(0));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.port(), None);
    }

    #[test]
    fn test_handshake_port_negative_rejected() {
        // Negative port should be rejected
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"p".to_vec(), BencodeValue::Int(-1));
        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.port(), None);
    }

    #[test]
    fn test_handshake_port_max_valid() {
        // port=65535 should be accepted
        let mut hs = ExtensionHandshake::new();
        hs.with_port(65535);
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.port(), Some(65535));
    }

    #[test]
    fn test_handshake_all_fields_roundtrip() {
        // Full handshake with all fields set — mirrors real C++ aria2 output
        let mut hs = ExtensionHandshake::new();
        hs.with_version("aria2/1.37.0")
            .with_port(6881)
            .with_metadata_size(45678)
            .with_reqq(1000)
            .with_ut_metadata(3)
            .with_ut_pex(4);

        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.v(), Some("aria2/1.37.0"));
        assert_eq!(parsed.port(), Some(6881));
        assert_eq!(parsed.metadata_size(), Some(45678));
        assert_eq!(parsed.reqq(), 1000);
        assert_eq!(parsed.ut_metadata_id(), Some(3));
        assert_eq!(parsed.ut_pex_id(), Some(4));
    }

    #[test]
    fn test_handshake_peer_message_with_all_fields() {
        // Simulate a peer message with all BEP 10 fields, matching C++ wire format
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(2));
        m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(3));

        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"v".to_vec(), BencodeValue::Bytes(b"qBittorrent/4.5.2".to_vec()));
        root.insert(b"p".to_vec(), BencodeValue::Int(6969));
        root.insert(b"metadata_size".to_vec(), BencodeValue::Int(98765));
        root.insert(b"reqq".to_vec(), BencodeValue::Int(250));

        let bytes = BencodeValue::Dict(root).encode();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.v(), Some("qBittorrent/4.5.2"));
        assert_eq!(parsed.port(), Some(6969));
        assert_eq!(parsed.metadata_size(), Some(98765));
        assert_eq!(parsed.reqq(), 250);
        assert_eq!(parsed.ut_metadata_id(), Some(2));
        assert_eq!(parsed.ut_pex_id(), Some(3));
    }
}
