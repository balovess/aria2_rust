//! BEP 5 compact peer and node encodings.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use super::message::DhtMessage;
use crate::bittorrent::bencode::codec::BencodeValue;

/// A compact DHT node address together with its 20-byte node ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactNode {
    V4(Ipv4Addr, u16),
    V6(Ipv6Addr, u16),
}

impl CompactNode {
    pub fn to_socket_addr(&self) -> SocketAddr {
        match self {
            Self::V4(ip, port) => SocketAddr::new(IpAddr::V4(*ip), *port),
            Self::V6(ip, port) => SocketAddr::V6(SocketAddrV6::new(*ip, *port, 0, 0)),
        }
    }
}

/// Extract compact peers from a BEP 5 response's `values` list.
pub fn extract_compact_peers_from_response(response: &DhtMessage) -> Vec<SocketAddr> {
    let Some(result) = &response.r else {
        return Vec::new();
    };
    let Some(BencodeValue::List(values)) = result.dict_get(b"values") else {
        return Vec::new();
    };

    values
        .iter()
        .filter_map(|value| {
            let bytes = value.as_bytes()?;
            match bytes.len() {
                6 => Some(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])),
                    u16::from_be_bytes([bytes[4], bytes[5]]),
                )),
                18 => Some(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[..16]).ok()?),
                    u16::from_be_bytes([bytes[16], bytes[17]]),
                    0,
                    0,
                ))),
                _ => None,
            }
        })
        .collect()
}

/// Extract standard BEP 5 compact nodes from a `find_node` or `get_peers`
/// response. `nodes` contains IPv4 records and `nodes6` contains IPv6
/// records; each record includes a 20-byte node ID before the address.
pub fn extract_compact_nodes_from_response(response: &DhtMessage) -> Vec<(SocketAddr, [u8; 20])> {
    let Some(result) = &response.r else {
        return Vec::new();
    };

    let mut nodes = Vec::new();
    if let Some(data) = result.dict_get(b"nodes").and_then(BencodeValue::as_bytes) {
        // Keep the length-based fallback for peers that place IPv6 records in
        // `nodes` instead of using the standard `nodes6` key.
        let ipv6 = if data.len().is_multiple_of(26) {
            false
        } else if data.len().is_multiple_of(38) {
            true
        } else {
            return nodes;
        };
        nodes.extend(
            parse_bep5_compact_nodes(data, ipv6)
                .into_iter()
                .map(|(node, node_id)| (node.to_socket_addr(), node_id)),
        );
    }
    if let Some(data) = result.dict_get(b"nodes6").and_then(BencodeValue::as_bytes) {
        nodes.extend(
            parse_bep5_compact_nodes(data, true)
                .into_iter()
                .map(|(node, node_id)| (node.to_socket_addr(), node_id)),
        );
    }
    nodes
}

/// Parse standard BEP 5 compact node records.
pub fn parse_bep5_compact_nodes(data: &[u8], ipv6: bool) -> Vec<(CompactNode, [u8; 20])> {
    let record_len = if ipv6 { 38 } else { 26 };
    let mut nodes = Vec::with_capacity(data.len() / record_len);
    for chunk in data.chunks_exact(record_len) {
        let mut node_id = [0u8; 20];
        node_id.copy_from_slice(&chunk[..20]);
        let node = if ipv6 {
            let address = <[u8; 16]>::try_from(&chunk[20..36])
                .expect("chunks_exact(38) guarantees the IPv6 address length");
            CompactNode::V6(
                Ipv6Addr::from(address),
                u16::from_be_bytes([chunk[36], chunk[37]]),
            )
        } else {
            CompactNode::V4(
                Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]),
                u16::from_be_bytes([chunk[24], chunk[25]]),
            )
        };
        nodes.push((node, node_id));
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn parses_bep5_ipv4_node() {
        let mut data = vec![0xAB; 20];
        data.extend_from_slice(&[10, 0, 0, 2]);
        data.extend_from_slice(&5000u16.to_be_bytes());

        let nodes = parse_bep5_compact_nodes(&data, false);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].0.to_socket_addr(),
            "10.0.0.2:5000".parse().unwrap()
        );
        assert_eq!(nodes[0].1, [0xAB; 20]);
    }

    #[test]
    fn parses_bep5_ipv6_node() {
        let mut data = vec![0xCD; 20];
        data.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        data.extend_from_slice(&6881u16.to_be_bytes());

        let nodes = parse_bep5_compact_nodes(&data, true);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].0.to_socket_addr(), "[::1]:6881".parse().unwrap());
        assert_eq!(nodes[0].1, [0xCD; 20]);
    }

    #[test]
    fn extracts_ipv4_and_ipv6_peers() {
        let mut result = BTreeMap::new();
        result.insert(
            b"values".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(vec![10, 0, 0, 1, 0x1A, 0xE1]),
                BencodeValue::Bytes(vec![0u8; 15].into_iter().chain([1u8, 0x1A, 0xE1]).collect()),
            ]),
        );
        let response = DhtMessage::new_response(vec![1, 2], BencodeValue::Dict(result));
        let peers = extract_compact_peers_from_response(&response);

        assert_eq!(
            peers,
            vec![
                "10.0.0.1:6881".parse().unwrap(),
                "[::1]:6881".parse().unwrap(),
            ]
        );
    }
}
