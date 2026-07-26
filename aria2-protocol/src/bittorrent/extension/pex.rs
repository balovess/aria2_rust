use std::collections::BTreeMap;

use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::peer::connection::PeerAddr;

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
    pub const EXTENSION_ID: u8 = 1;
    const COMPACT_PEER_SIZE_V4: usize = 6;
    const COMPACT_PEER_SIZE_V6: usize = 18;
    pub const DEFAULT_MAX_PEERS: usize = 50; // Used as default for build_pex_added() max_peers param

    pub fn parse_pex_data(data: &[u8]) -> Result<PexMessage, String> {
        let (value, _) = BencodeValue::decode(data)
            .map_err(|e| format!("Failed to decode PEX bencode: {}", e))?;

        if !value.is_dict() {
            return Err("PEX message must be a bencoded dictionary".to_string());
        }

        // IPv4 added peers (BEP 11 "added" key)
        let mut added_peers = Vec::new();
        if let Some(added_data) = value.dict_get("added").and_then(|v| v.as_bytes()) {
            added_peers = decode_compact_peers_v4(added_data)?;
        }

        // IPv4 added flags (BEP 11 "added.f" key)
        let mut flags = Vec::new();
        if let Some(flags_data) = value.dict_get("added.f").and_then(|v| v.as_bytes()) {
            flags = flags_data.to_vec();
        }

        // IPv6 added peers (BEP 11 "added6" key)
        if let Some(added6_data) = value.dict_get("added6").and_then(|v| v.as_bytes()) {
            let v6_peers = decode_compact_peers_v6(added6_data)?;
            added_peers.extend(v6_peers);
        }

        // IPv6 added flags (BEP 11 "added6.f" key)
        if let Some(flags6_data) = value.dict_get("added6.f").and_then(|v| v.as_bytes()) {
            // IPv6 flags are appended after IPv4 flags, matching the peer order
            let v4_count = flags.len();
            let v6_count = flags6_data.len();
            flags.resize(v4_count + v6_count, 0u8);
            flags[v4_count..].copy_from_slice(flags6_data);
        }

        // IPv4 dropped peers (BEP 11 "dropped" key)
        let mut dropped_peers =
            if let Some(dropped_data) = value.dict_get("dropped").and_then(|v| v.as_bytes()) {
                decode_compact_peers_v4(dropped_data)?
            } else {
                Vec::new()
            };

        // IPv6 dropped peers (BEP 11 "dropped6" key)
        if let Some(dropped6_data) = value.dict_get("dropped6").and_then(|v| v.as_bytes()) {
            let v6_dropped = decode_compact_peers_v6(dropped6_data)?;
            dropped_peers.extend(v6_dropped);
        }

        let peers_with_flags: Vec<PexAddedPeer> = added_peers
            .into_iter()
            .enumerate()
            .map(|(i, addr)| PexAddedPeer {
                addr,
                flags: flags.get(i).copied().unwrap_or(0),
            })
            .collect();

        Ok(PexMessage::Added {
            peers: peers_with_flags,
            dropped: dropped_peers,
        })
    }

    pub fn build_pex_message(added: &[PeerAddr], removed: &[PeerAddr]) -> BencodeValue {
        let mut dict = BTreeMap::new();

        // Separate IPv4 and IPv6 peers per BEP 11
        let (added_v4, added_v6) = partition_peers_by_ip(added);
        let (dropped_v4, dropped_v6) = partition_peers_by_ip(removed);

        // IPv4 added peers + flags
        if !added_v4.is_empty() {
            let compact = encode_compact_peers_v4(&added_v4);
            let flags = vec![0u8; added_v4.len()];
            dict.insert(b"added".to_vec(), BencodeValue::Bytes(compact));
            dict.insert(b"added.f".to_vec(), BencodeValue::Bytes(flags));
        }

        // IPv6 added peers + flags
        if !added_v6.is_empty() {
            let compact = encode_compact_peers_v6(&added_v6);
            let flags = vec![0u8; added_v6.len()];
            dict.insert(b"added6".to_vec(), BencodeValue::Bytes(compact));
            dict.insert(b"added6.f".to_vec(), BencodeValue::Bytes(flags));
        }

        // IPv4 dropped peers
        if !dropped_v4.is_empty() {
            let compact = encode_compact_peers_v4(&dropped_v4);
            dict.insert(b"dropped".to_vec(), BencodeValue::Bytes(compact));
        }

        // IPv6 dropped peers
        if !dropped_v6.is_empty() {
            let compact = encode_compact_peers_v6(&dropped_v6);
            dict.insert(b"dropped6".to_vec(), BencodeValue::Bytes(compact));
        }

        BencodeValue::Dict(dict)
    }

    pub fn is_supported_by_peer(extension_ids: &[Option<u8>]) -> bool {
        extension_ids.contains(&Some(Self::EXTENSION_ID))
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

    /// Encrypt outgoing PEX payload using MSE stream cipher (RC4)
    ///
    /// Uses the existing MseCryptoState from encrypted_connection.rs to encrypt
    /// PEX messages when MSE negotiation has been completed with a peer.
    ///
    /// # Arguments
    /// * `payload` - The raw PEX bencoded message bytes to encrypt
    /// * `cipher` - Mutable reference to the MSE crypto state (contains RC4 cipher)
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Encrypted payload ready for transmission
    /// * `Err(String)` - Encryption failure description
    pub fn encrypt_payload(payload: &[u8], cipher: &mut MseCryptoState) -> Result<Vec<u8>, String> {
        if !cipher.is_encrypted() {
            return Ok(payload.to_vec());
        }

        let mut encrypted = payload.to_vec();
        cipher.encrypt(&mut encrypted);
        Ok(encrypted)
    }

    /// Decrypt incoming PEX payload using MSE stream cipher (RC4)
    ///
    /// Uses the existing MseCryptoState to decrypt PEX messages received
    /// from peers that have completed MSE handshake.
    ///
    /// # Arguments
    /// * `encrypted` - The encrypted PEX payload received from peer
    /// * `cipher` - Mutable reference to the MSE crypto state (contains RC4 cipher)
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Decrypted PEX payload ready for parsing
    /// * `Err(String)` - Decryption failure description
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

/// Partition peers into IPv4 and IPv6 lists based on address family.
fn partition_peers_by_ip(peers: &[PeerAddr]) -> (Vec<PeerAddr>, Vec<PeerAddr>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for peer in peers {
        if peer.ip.parse::<std::net::Ipv4Addr>().is_ok() {
            v4.push(peer.clone());
        } else if peer.ip.parse::<std::net::Ipv6Addr>().is_ok() {
            v6.push(peer.clone());
        }
    }
    (v4, v6)
}

/// Encode IPv4 peers into compact format (4-byte IP + 2-byte port per peer).
fn encode_compact_peers_v4(peers: &[PeerAddr]) -> Vec<u8> {
    let mut result = Vec::with_capacity(peers.len() * PexHandler::COMPACT_PEER_SIZE_V4);
    for peer in peers {
        if let Ok(ipv4) = peer.ip.parse::<std::net::Ipv4Addr>() {
            let mut buf = [0u8; 6];
            buf[..4].copy_from_slice(&ipv4.octets());
            buf[4..6].copy_from_slice(&peer.port.to_be_bytes());
            result.extend_from_slice(&buf);
        }
    }
    result
}

/// Encode IPv6 peers into compact format (16-byte IP + 2-byte port per peer).
fn encode_compact_peers_v6(peers: &[PeerAddr]) -> Vec<u8> {
    let mut result = Vec::with_capacity(peers.len() * PexHandler::COMPACT_PEER_SIZE_V6);
    for peer in peers {
        if let Ok(ipv6) = peer.ip.parse::<std::net::Ipv6Addr>() {
            let mut buf = [0u8; 18];
            buf[..16].copy_from_slice(&ipv6.octets());
            buf[16..18].copy_from_slice(&peer.port.to_be_bytes());
            result.extend_from_slice(&buf);
        }
    }
    result
}

/// Decode compact IPv4 peer data (6 bytes per peer).
fn decode_compact_peers_v4(data: &[u8]) -> Result<Vec<PeerAddr>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if !data.len().is_multiple_of(PexHandler::COMPACT_PEER_SIZE_V4) {
        return Err(format!(
            "Invalid compact IPv4 peer data length: {} (must be multiple of {})",
            data.len(),
            PexHandler::COMPACT_PEER_SIZE_V4
        ));
    }
    let count = data.len() / PexHandler::COMPACT_PEER_SIZE_V4;
    let mut peers = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * PexHandler::COMPACT_PEER_SIZE_V4;
        let end = start + PexHandler::COMPACT_PEER_SIZE_V4;
        let peer = PeerAddr::from_compact(&data[start..end])
            .ok_or_else(|| format!("Failed to parse IPv4 peer at index {}", i))?;
        peers.push(peer);
    }
    Ok(peers)
}

/// Decode compact IPv6 peer data (18 bytes per peer).
fn decode_compact_peers_v6(data: &[u8]) -> Result<Vec<PeerAddr>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    if !data.len().is_multiple_of(PexHandler::COMPACT_PEER_SIZE_V6) {
        return Err(format!(
            "Invalid compact IPv6 peer data length: {} (must be multiple of {})",
            data.len(),
            PexHandler::COMPACT_PEER_SIZE_V6
        ));
    }
    let count = data.len() / PexHandler::COMPACT_PEER_SIZE_V6;
    let mut peers = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * PexHandler::COMPACT_PEER_SIZE_V6;
        let end = start + PexHandler::COMPACT_PEER_SIZE_V6;
        let peer = decode_ipv6_peer(&data[start..end])
            .ok_or_else(|| format!("Failed to parse IPv6 peer at index {}", i))?;
        peers.push(peer);
    }
    Ok(peers)
}

fn decode_ipv6_peer(data: &[u8]) -> Option<PeerAddr> {
    if data.len() < 18 {
        return None;
    }

    let ip_bytes: [u8; 16] = data[..16].try_into().ok()?;
    let ipv6 = std::net::Ipv6Addr::from(ip_bytes);
    let port = u16::from_be_bytes([data[16], data[17]]);

    Some(PeerAddr {
        ip: ipv6.to_string(),
        port,
    })
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
mod tests;
