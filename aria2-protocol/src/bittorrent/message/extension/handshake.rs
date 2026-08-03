//! BEP 10 Extension Protocol handshake message types for BitTorrent.
//!
//! Implements the wire-format encoding/decoding for the **Extension Handshake**
//! (BEP 10), which negotiates supported extensions (e.g. `ut_metadata`,
//! `ut_pex`) with a peer.
//!
//! This type operates on the *payload* portion of a `BtMessage::Extended`,
//! i.e. the bytes **after** the 1-byte `ext_id` field. The `ext_id` itself
//! is handled at the `BtMessage` layer.

use std::collections::BTreeMap;

use tracing::debug;

use crate::bittorrent::bencode::codec::BencodeValue;

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
        root.insert(b"reqq".to_vec(), BencodeValue::Int(self.reqq as i64));

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

        let dict = val
            .as_dict()
            .ok_or("Extension handshake payload is not a dict")?;

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
        root.insert(
            b"metadata_size".to_vec(),
            BencodeValue::Int(oversized as i64),
        );
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
        root.insert(
            b"v".to_vec(),
            BencodeValue::Bytes(b"qBittorrent/4.5.2".to_vec()),
        );
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
