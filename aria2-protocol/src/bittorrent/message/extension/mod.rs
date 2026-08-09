//! BEP extension message types for BitTorrent (`BtMessage::Extended` payloads).
//!
//! This module groups the per-protocol extension message types negotiated and
//! exchanged over the BEP 10 extension mechanism:
//!
//! - [`handshake`] — BEP 10 extension handshake (negotiate `ut_metadata`, `ut_pex`)
//! - [`ut_metadata`] — BEP 9 metadata piece exchange
//! - [`ut_pex`] — BEP 11 peer exchange (compact IPv4/IPv6 peers)
//!
//! Each submodule operates on the *payload* portion of a `BtMessage::Extended`,
//! i.e. the bytes **after** the 1-byte `ext_id` field.

mod handshake;
mod ut_metadata;
mod ut_pex;

pub use handshake::*;
pub use ut_metadata::*;
pub use ut_pex::*;

/// Compact peer size constants (BEP 11 wire format).
pub(crate) const COMPACT_PEER_V4_SIZE: usize = 6;
pub(crate) const COMPACT_PEER_V6_SIZE: usize = 18;

/// Decode compact IPv4 peer data (6 bytes per peer).
///
/// Returns an error if `data` length is not a multiple of
/// [`COMPACT_PEER_V4_SIZE`]. Used by `ut_pex` when parsing the `added` /
/// `dropped` keys.
pub(crate) fn decode_compact_v4(data: &[u8]) -> Result<Vec<CompactPeerV4>, String> {
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
///
/// Returns an error if `data` length is not a multiple of
/// [`COMPACT_PEER_V6_SIZE`]. Used by `ut_pex` when parsing the `added6` /
/// `dropped6` keys.
pub(crate) fn decode_compact_v6(data: &[u8]) -> Result<Vec<CompactPeerV6>, String> {
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
