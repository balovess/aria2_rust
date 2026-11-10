//! High-level BEP 11 peer-list operations.
//!
//! Wire encoding, decoding, and compact-address representation are owned by
//! [`UtPexMessage`](crate::bittorrent::message::extension::UtPexMessage).

use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::message::extension::{DEFAULT_UT_PEX_ID, UtPexMessage};
use crate::bittorrent::peer::address::PeerAddr;

const LEGACY_EXTENSION_ID: u8 = 1;

#[derive(Debug, Clone)]
pub struct PexAddedPeer {
    pub addr: PeerAddr,
    pub flags: u8,
}

/// BEP 11 Peer Exchange message.
///
/// Carries both newly connected peers (`added`) and disconnected peers
/// (`dropped`). The full BEP 11 protocol requires both fields so that
/// peers can learn about peers that have left the swarm.
#[derive(Debug, Clone)]
pub enum PexMessage {
    /// Standard PEX message with both added and dropped peers.
    Added {
        peers: Vec<PexAddedPeer>,
        dropped: Vec<PeerAddr>,
    },
    /// Legacy message with only dropped peers (rare in practice).
    Removed(Vec<PeerAddr>),
}

pub struct PexHandler;

impl PexHandler {
    pub const EXTENSION_NAME: &'static str = "ut_pex";
    /// Default `ut_pex` ID advertised by this implementation's BEP 10 handshake.
    ///
    /// A peer's ID is negotiated independently and must be read from that
    /// peer's extension handshake.
    pub const DEFAULT_EXTENSION_ID: u8 = DEFAULT_UT_PEX_ID;
    /// Historical ID used by the legacy static-ID helper.
    ///
    /// This is not a negotiated BEP 10 ID and must not be used to identify a
    /// remote peer's `ut_pex` extension.
    #[deprecated(note = "remote ut_pex IDs are negotiated per peer")]
    pub const EXTENSION_ID: u8 = LEGACY_EXTENSION_ID;
    pub const DEFAULT_MAX_PEERS: usize = 50; // Used as default for build_pex_added() max_peers param

    pub fn parse_pex_data(data: &[u8]) -> Result<PexMessage, String> {
        let (value, _) = BencodeValue::decode(data)
            .map_err(|e| format!("Failed to decode PEX bencode: {}", e))?;
        if value.as_dict().is_none() {
            return Err("PEX message must be a bencoded dictionary".to_string());
        }
        let wire_message = UtPexMessage::from_bencode_value(&value)?;
        let mut added_peers =
            Vec::with_capacity(wire_message.added.len() + wire_message.added6.len());
        let mut flags = wire_message.added_f.clone();
        if !wire_message.added6_f.is_empty() {
            let flags_len = flags.len();
            flags.resize(flags_len + wire_message.added6_f.len(), 0);
            flags[flags_len..].copy_from_slice(&wire_message.added6_f);
        }
        added_peers.extend(wire_message.added.iter().map(|peer| peer.to_peer_addr()));
        added_peers.extend(wire_message.added6.iter().map(|peer| peer.to_peer_addr()));
        let peers_with_flags = added_peers
            .into_iter()
            .enumerate()
            .map(|(index, addr)| PexAddedPeer {
                addr,
                flags: flags.get(index).copied().unwrap_or(0),
            })
            .collect();
        let mut dropped_peers = wire_message
            .dropped
            .iter()
            .map(|peer| peer.to_peer_addr())
            .collect::<Vec<_>>();
        dropped_peers.extend(wire_message.dropped6.iter().map(|peer| peer.to_peer_addr()));

        Ok(PexMessage::Added {
            peers: peers_with_flags,
            dropped: dropped_peers,
        })
    }

    pub fn build_pex_message(added: &[PeerAddr], removed: &[PeerAddr]) -> BencodeValue {
        UtPexMessage::from_peer_addrs(added, removed).to_bencode_value()
    }

    /// Whether a remote peer advertised a usable `ut_pex` extension ID.
    ///
    /// Pass the `ut_pex` value from that peer's BEP 10 `m` dictionary. BEP 10
    /// uses ID zero to disable an extension.
    pub fn is_supported_by_peer_id(remote_ut_pex_id: Option<u8>) -> bool {
        remote_ut_pex_id.is_some_and(|id| id != 0)
    }

    /// Legacy static-ID check. An ID list has no extension-name mapping, so it
    /// cannot determine whether a peer supports `ut_pex`; use
    /// [`Self::is_supported_by_peer_id`] with the peer's negotiated `ut_pex`
    /// value instead.
    #[deprecated(note = "use is_supported_by_peer_id with the negotiated ut_pex ID")]
    pub fn is_supported_by_peer(extension_ids: &[Option<u8>]) -> bool {
        extension_ids.contains(&Some(LEGACY_EXTENSION_ID))
    }

    pub fn build_pex_added(
        known_peers: &[PeerAddr],
        remote_addr: &PeerAddr,
        max_peers: usize,
    ) -> BencodeValue {
        let filtered: Vec<PeerAddr> = known_peers
            .iter()
            .filter(|peer| **peer != *remote_addr)
            .take(max_peers)
            .cloned()
            .collect();

        Self::build_pex_message(&filtered, &[])
    }

    pub fn process_received_pex(
        data: &[u8],
        local_addr: &PeerAddr,
    ) -> Result<(Vec<PeerAddr>, Vec<PeerAddr>), String> {
        let msg = Self::parse_pex_data(data)?;

        match msg {
            PexMessage::Added { peers, dropped } => {
                let added: Vec<PeerAddr> = peers
                    .into_iter()
                    .map(|p| p.addr)
                    .filter(|addr| addr != local_addr)
                    .collect();

                let dropped_filtered: Vec<PeerAddr> = dropped
                    .into_iter()
                    .filter(|addr| addr != local_addr)
                    .collect();

                let added_deduped = deduplicate_peers(&added);
                let dropped_deduped = deduplicate_peers(&dropped_filtered);
                Ok((added_deduped, dropped_deduped))
            }
            PexMessage::Removed(peers) => {
                let filtered: Vec<PeerAddr> = peers
                    .into_iter()
                    .filter(|addr| addr != local_addr)
                    .collect();
                let deduped = deduplicate_peers(&filtered);
                Ok((vec![], deduped))
            }
        }
    }

    /// Encrypt standalone bytes with an MSE stream cipher.
    ///
    /// This legacy helper is not suitable for peer traffic: MSE encrypts the
    /// complete BitTorrent wire stream in `PeerConnection`. Calling this on a
    /// PEX payload would advance the same stream cipher twice and corrupt the
    /// connection. Use the peer connection send path instead.
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Encrypted payload ready for transmission
    /// * `Err(String)` - Encryption failure description
    #[deprecated(note = "MSE encryption belongs to the complete PeerConnection wire stream")]
    pub fn encrypt_payload(payload: &[u8], cipher: &mut MseCryptoState) -> Result<Vec<u8>, String> {
        if !cipher.is_encrypted() {
            return Ok(payload.to_vec());
        }

        let mut encrypted = payload.to_vec();
        cipher.encrypt(&mut encrypted);
        Ok(encrypted)
    }

    /// Decrypt standalone bytes with an MSE stream cipher.
    ///
    /// This legacy helper is not suitable for peer traffic: MSE decryption is
    /// performed for the complete BitTorrent wire stream in `PeerConnection`.
    /// Calling this on an extracted PEX payload would corrupt the cipher state.
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Decrypted PEX payload ready for parsing
    /// * `Err(String)` - Decryption failure description
    #[deprecated(note = "MSE decryption belongs to the complete PeerConnection wire stream")]
    pub fn decrypt_payload(
        encrypted: &[u8],
        cipher: &mut MseCryptoState,
    ) -> Result<Vec<u8>, String> {
        if !cipher.is_encrypted() {
            return Ok(encrypted.to_vec());
        }

        let mut decrypted = encrypted.to_vec();
        cipher.decrypt(&mut decrypted);
        Ok(decrypted)
    }
}

fn deduplicate_peers(peers: &[PeerAddr]) -> Vec<PeerAddr> {
    let mut seen = std::collections::HashSet::new();
    peers
        .iter()
        .filter(|peer| seen.insert((peer.ip.clone(), peer.port)))
        .cloned()
        .collect()
}

#[cfg(test)]
fn encode_compact_peers_v4(peers: &[PeerAddr]) -> Vec<u8> {
    UtPexMessage::from_peer_addrs(peers, &[])
        .added
        .into_iter()
        .flat_map(|peer| peer.0)
        .collect()
}

#[cfg(test)]
fn encode_compact_peers_v6(peers: &[PeerAddr]) -> Vec<u8> {
    UtPexMessage::from_peer_addrs(peers, &[])
        .added6
        .into_iter()
        .flat_map(|peer| peer.0)
        .collect()
}

#[cfg(test)]
fn decode_compact_peers_v4(data: &[u8]) -> Result<Vec<PeerAddr>, String> {
    let mut dict = std::collections::BTreeMap::new();
    dict.insert(b"added".to_vec(), BencodeValue::Bytes(data.to_vec()));
    let payload = BencodeValue::Dict(dict).encode();
    Ok(UtPexMessage::from_payload(&payload)?
        .added
        .iter()
        .map(|peer| peer.to_peer_addr())
        .collect())
}

#[cfg(test)]
fn decode_compact_peers_v6(data: &[u8]) -> Result<Vec<PeerAddr>, String> {
    let mut dict = std::collections::BTreeMap::new();
    dict.insert(b"added6".to_vec(), BencodeValue::Bytes(data.to_vec()));
    let payload = BencodeValue::Dict(dict).encode();
    Ok(UtPexMessage::from_payload(&payload)?
        .added6
        .iter()
        .map(|peer| peer.to_peer_addr())
        .collect())
}

#[cfg(test)]
mod tests;
