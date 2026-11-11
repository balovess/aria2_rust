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
/// Default values use this implementation's peer agent with
/// `ut_metadata = 1`, `ut_pex = 2`, and `reqq = 500`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionHandshake {
    /// The `m` sub-dictionary mapping extension names to their negotiated ext_id.
    m_dict: BTreeMap<Vec<u8>, BencodeValue>,
    /// Client version string (BEP 10 `v` key).
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
/// Default `ut_pex` ID advertised by this implementation.
pub const DEFAULT_UT_PEX_ID: u8 = 2;
/// Default maximum outstanding metadata requests (reqq).
const DEFAULT_REQQ: u32 = 500;
/// Default client version string sent in the `v` key.
const DEFAULT_CLIENT_VERSION: &str = crate::identity::DEFAULT_PEER_AGENT;
/// Maximum BEP 9 metadata size accepted from peers (8 MiB).
///
/// This is an interoperability and resource-safety limit shared with the core
/// metadata collector.
pub const MAX_UT_METADATA_SIZE: u32 = 8 * 1024 * 1024;

impl ExtensionHandshake {
    /// Create a new handshake with default values:
    /// ut_metadata=1, ut_pex=2, reqq=500, and the product peer agent.
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

        // BEP 10 makes each top-level field optional. Treat a missing or
        // malformed `m` as an empty extension table, matching aria2's
        // `downcast<Dict>(dict->get("m"))` behavior without discarding other
        // valid handshake fields.
        let m_dict = dict
            .get(b"m".as_slice())
            .and_then(BencodeValue::as_dict)
            .cloned()
            .unwrap_or_default();

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
            .filter(|&i| i > 0 && i <= MAX_UT_METADATA_SIZE as i64)
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
mod tests;
