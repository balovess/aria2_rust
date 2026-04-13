use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tracing::debug;

use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::transaction::TransactionManager;
use crate::bittorrent::bencode::codec::BencodeValue;

/// Represents a compact node address extracted from DHT responses.
/// Supports both IPv4 (type 0x04) and IPv6 (type 0x06) formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactNode {
    /// IPv4 node: IP address + port
    V4(std::net::Ipv4Addr, u16),
    /// IPv6 node: IP address + port
    V6(std::net::Ipv6Addr, u16),
}

impl CompactNode {
    /// Convert to a standard SocketAddr
    pub fn to_socket_addr(&self) -> SocketAddr {
        match self {
            CompactNode::V4(ip, port) => SocketAddr::from((*ip, *port)),
            CompactNode::V6(ip, port) => {
                SocketAddr::V6(std::net::SocketAddrV6::new(*ip, *port, 0, 0))
            }
        }
    }
}

/// Parse compact nodes from raw data using type-byte detection.
///
/// DHT get_peers responses may return nodes in a format where each entry is prefixed
/// by an address type byte:
/// - Type 0x04 = IPv4: type(1) + IP(4) + port(2) = 7 bytes per node
/// - Type 0x06 = IPv6: type(1) + IP(16) + port(2) = 19 bytes per node
///
/// This function handles mixed IPv4/IPv6 responses correctly.
///
/// # Arguments
/// * `data` - Raw bytes containing compact node data
/// * `start` - Starting offset within the data buffer
/// * `count` - Maximum number of nodes to parse
///
/// # Returns
/// A vector of parsed [`CompactNode`] entries
pub fn parse_compact_nodes(data: &[u8], start: usize, count: usize) -> Vec<CompactNode> {
    let mut nodes = Vec::new();
    let mut offset = start;

    for _ in 0..count {
        if offset >= data.len() {
            break;
        }

        let node_type = data[offset];
        offset += 1;

        match node_type {
            0x04 => {
                // IPv4: 4 bytes IP + 2 bytes port = 6 bytes total after type byte
                if offset + 6 > data.len() {
                    break;
                }
                let ip = std::net::Ipv4Addr::new(
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                );
                let port = u16::from_be_bytes([data[offset + 4], data[offset + 5]]);
                nodes.push(CompactNode::V4(ip, port));
                offset += 6;
            }
            0x06 => {
                // IPv6: 16 bytes IP + 2 bytes port = 18 bytes total after type byte
                if offset + 18 > data.len() {
                    break;
                }
                let ip = std::net::Ipv6Addr::from([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                    data[offset + 5],
                    data[offset + 6],
                    data[offset + 7],
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                    data[offset + 12],
                    data[offset + 13],
                    data[offset + 14],
                    data[offset + 15],
                ]);
                let port = u16::from_be_bytes([data[offset + 16], data[offset + 17]]);
                nodes.push(CompactNode::V6(ip, port));
                offset += 18;
            }
            _ => {
                // Unknown type byte - stop parsing
                debug!("Unknown compact node type byte: 0x{:02X}", node_type);
                break;
            }
        }
    }

    nodes
}

pub struct DhtClientConfig {
    pub self_id: [u8; 20],
    pub bootstrap_nodes: Vec<SocketAddr>,
    pub max_concurrent_queries: usize,
    pub query_timeout: Duration,
    pub max_rounds: usize,
}

impl Default for DhtClientConfig {
    fn default() -> Self {
        Self {
            self_id: [0u8; 20],
            bootstrap_nodes: vec![],
            max_concurrent_queries: 8,
            query_timeout: Duration::from_secs(5),
            max_rounds: 3,
        }
    }
}

pub struct DiscoveredPeers {
    pub addresses: Vec<SocketAddr>,
    pub nodes_contacted: usize,
}

/// Cache entry for nodes discovered during find_peers queries.
/// Stores discovered peer addresses and the timestamp of discovery.
struct NodeCacheEntry {
    /// Peer addresses discovered for this info_hash
    peers: Vec<SocketAddr>,
    /// When this cache entry was created or last updated
    timestamp: Instant,
    /// How many nodes were contacted to get these results
    nodes_contacted: usize,
}

/// Default TTL for node cache entries (5 minutes)
const NODE_CACHE_TTL_SECS: u64 = 300;

pub struct DhtClient {
    config: DhtClientConfig,
    routing_table: RoutingTable,
    #[allow(dead_code)] // Reserved for future DHT transaction management
    tx_manager: TransactionManager,
    /// Cache of discovered peers keyed by info_hash
    node_cache: HashMap<[u8; 20], NodeCacheEntry>,
}

impl DhtClient {
    pub fn new(config: DhtClientConfig) -> Self {
        let mut routing_table = RoutingTable::new(config.self_id);
        for addr in &config.bootstrap_nodes {
            let node = DhtNode::new([0u8; 20], *addr);
            routing_table.insert(node);
        }

        Self {
            config,
            routing_table,
            tx_manager: TransactionManager::new(),
            node_cache: HashMap::new(),
        }
    }

    pub async fn discover_peers(
        &mut self,
        target_info_hash: &[u8; 20],
    ) -> Result<DiscoveredPeers, String> {
        // Check cache first for valid (non-expired) entries
        if let Some(entry) = self.node_cache.get(target_info_hash) {
            if entry.timestamp.elapsed().as_secs() < NODE_CACHE_TTL_SECS {
                debug!(
                    "DHT: returning cached peers for info_hash ({} peers, age={}s)",
                    entry.peers.len(),
                    entry.timestamp.elapsed().as_secs()
                );
                return Ok(DiscoveredPeers {
                    addresses: entry.peers.clone(),
                    nodes_contacted: entry.nodes_contacted,
                });
            } else {
                // Expired entry - remove it
                self.node_cache.remove(target_info_hash);
            }
        }

        let socket = DhtSocket::bind(0).await?;
        let mut all_peers: Vec<SocketAddr> = Vec::new();
        let mut nodes_contacted = 0usize;

        for round in 0..self.config.max_rounds {
            debug!(
                "DHT discovery round {}/{}",
                round + 1,
                self.config.max_rounds
            );

            if !all_peers.is_empty() {
                break;
            }

            let closest_nodes = self
                .routing_table
                .find_closest(target_info_hash, self.config.max_concurrent_queries);
            let nodes_to_query: Vec<SocketAddr> = if !closest_nodes.is_empty() {
                closest_nodes.iter().map(|n| n.addr).collect()
            } else if round == 0 {
                self.config.bootstrap_nodes.clone()
            } else {
                vec![]
            };
            if nodes_to_query.is_empty() {
                break;
            }

            let mut handles = Vec::new();
            for (i, node_addr) in nodes_to_query.iter().enumerate() {
                if i >= self.config.max_concurrent_queries {
                    break;
                }
                if *node_addr == SocketAddr::from(([0, 0, 0, 0], 0)) {
                    continue;
                }

                let tx_id = (round * 1000 + i) as u32;

                let query_msg =
                    DhtMessageBuilder::get_peers(tx_id, &self.config.self_id, target_info_hash);
                let addr = *node_addr;
                let query_timeout = self.config.query_timeout;
                let sock = socket.clone();

                handles.push(tokio::spawn(async move {
                    Self::query_node_with_socket(&sock, addr, &query_msg, query_timeout).await
                }));
            }

            for handle in handles {
                match handle.await {
                    Ok(Ok(response)) => {
                        nodes_contacted += 1;
                        let peers = extract_compact_peers_from_response(&response);
                        all_peers.extend(peers);

                        let new_nodes = extract_compact_nodes_from_response(&response);
                        for (naddr, nid) in new_nodes {
                            let node = DhtNode::new(nid, naddr);
                            self.routing_table.insert(node);
                        }
                    }
                    Ok(Err(e)) => {
                        debug!("DHT query error: {}", e);
                    }
                    Err(join_err) => {
                        debug!("DHT query task panicked: {}", join_err);
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        all_peers.sort();
        all_peers.dedup();

        // Store results in cache for future lookups
        self.node_cache.insert(
            *target_info_hash,
            NodeCacheEntry {
                peers: all_peers.clone(),
                timestamp: Instant::now(),
                nodes_contacted,
            },
        );

        Ok(DiscoveredPeers {
            addresses: all_peers,
            nodes_contacted,
        })
    }

    async fn query_node_with_socket(
        socket: &DhtSocket,
        node_addr: SocketAddr,
        message: &DhtMessage,
        query_timeout: Duration,
    ) -> Result<DhtMessage, String> {
        let encoded = message.encode()?;
        socket.send_to(node_addr, &encoded).await?;

        let mut buf = [0u8; 4096];
        match socket.recv_with_timeout(&mut buf, query_timeout).await {
            Ok((len, _from)) => {
                if len == 0 {
                    return Err("Empty response".to_string());
                }
                DhtMessage::decode(&buf[..len])
            }
            Err(e) => Err(e),
        }
    }

    pub fn routing_table(&self) -> &RoutingTable {
        &self.routing_table
    }
}

pub fn extract_compact_peers_from_response(response: &DhtMessage) -> Vec<SocketAddr> {
    let r = match &response.r {
        Some(r) => r,
        None => return vec![],
    };

    let values = match r.dict_get(b"values") {
        Some(BencodeValue::List(list)) => list,
        _ => return vec![],
    };

    let mut peers = Vec::new();
    for item in values {
        if let BencodeValue::Bytes(bytes) = item {
            if bytes.len() == 6 {
                // IPv4 compact peer
                let ip_bytes: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
                let port = u16::from_be_bytes([bytes[4], bytes[5]]);
                peers.push(SocketAddr::from((std::net::Ipv4Addr::from(ip_bytes), port)));
            } else if bytes.len() == 18 {
                // IPv6 compact peer
                let octets: [u8; 16] = match bytes[..16].try_into() {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                let port = u16::from_be_bytes([bytes[16], bytes[17]]);
                peers.push(SocketAddr::V6(std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::from(octets),
                    port,
                    0,
                    0,
                )));
            }
            // else: ignore unknown format
        }
    }
    peers
}

pub fn extract_compact_nodes_from_response(response: &DhtMessage) -> Vec<(SocketAddr, [u8; 20])> {
    let r = match &response.r {
        Some(r) => r,
        None => return vec![],
    };

    let nodes_data = match r.dict_get(b"nodes") {
        Some(BencodeValue::Bytes(data)) => data,
        _ => return vec![],
    };

    let mut nodes = Vec::new();

    if nodes_data.is_empty() {
        return nodes;
    }

    // Detect format: if total length is multiple of 38 -> likely all IPv6 nodes
    if nodes_data.len() % 38 == 0 && nodes_data.len() >= 38 {
        // All IPv6 nodes (20 ID + 16 IP + 2 port)
        for chunk in nodes_data.chunks(38) {
            if chunk.len() < 38 {
                continue;
            }
            let mut node_id = [0u8; 20];
            node_id.copy_from_slice(&chunk[0..20]);
            let octets: [u8; 16] = match chunk[20..36].try_into() {
                Ok(o) => o,
                Err(_) => continue,
            };
            let port = u16::from_be_bytes([chunk[36], chunk[37]]);
            nodes.push((
                SocketAddr::V6(std::net::SocketAddrV6::new(
                    std::net::Ipv6Addr::from(octets),
                    port,
                    0,
                    0,
                )),
                node_id,
            ));
        }
    } else {
        // Default: try IPv4 (26 bytes per entry) with fallback
        let chunk_size = 26;
        for chunk in nodes_data.chunks(chunk_size) {
            if chunk.len() < chunk_size {
                continue;
            }
            let mut node_id = [0u8; 20];
            node_id.copy_from_slice(&chunk[0..20]);
            let ip_bytes: [u8; 4] = [chunk[20], chunk[21], chunk[22], chunk[23]];
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            nodes.push((
                SocketAddr::from((std::net::Ipv4Addr::from(ip_bytes), port)),
                node_id,
            ));
        }
    }

    nodes
}

pub fn generate_random_node_id() -> [u8; 20] {
    use rand::RngCore;
    let mut id = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_compact_peers_empty() {
        let msg = DhtMessage::new_response(
            vec![1, 2],
            crate::bittorrent::bencode::codec::BencodeValue::Dict(
                std::collections::BTreeMap::new()
                    .into_iter()
                    .map(|(k, v): (&Vec<u8>, &Vec<u8>)| {
                        (
                            k.to_vec(),
                            crate::bittorrent::bencode::codec::BencodeValue::Bytes(v.clone()),
                        )
                    })
                    .collect::<std::collections::BTreeMap<Vec<u8>, _>>(),
            ),
        );

        let result = extract_compact_peers_from_response(&msg);
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_compact_peers_single() {
        use crate::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let peer_bytes: Vec<u8> = vec![
            192, 168, 1, 1, // IP: 192.168.1.1
            0x1F, 0x90, // Port: 8080
        ];

        let mut r_dict = BTreeMap::new();
        r_dict.insert(
            b"values".to_vec(),
            BencodeValue::List(vec![BencodeValue::Bytes(peer_bytes)]),
        );

        let msg = DhtMessage::new_response(vec![1, 2], BencodeValue::Dict(r_dict));
        let peers = extract_compact_peers_from_response(&msg);
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0].ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(peers[0].port(), 8080);
    }

    #[test]
    fn test_extract_compact_peers_multiple() {
        use crate::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let peer1: Vec<u8> = vec![10, 0, 0, 1, 0x00, 0x50];
        let peer2: Vec<u8> = vec![172, 16, 5, 1, 0x17, 0x70];

        let mut r_dict = BTreeMap::new();
        r_dict.insert(
            b"values".to_vec(),
            BencodeValue::List(vec![BencodeValue::Bytes(peer1), BencodeValue::Bytes(peer2)]),
        );

        let msg = DhtMessage::new_response(vec![1, 2], BencodeValue::Dict(r_dict));
        let peers = extract_compact_peers_from_response(&msg);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].port(), 80);
        assert_eq!(peers[1].port(), 6000);
    }

    #[test]
    fn test_extract_compact_nodes_single() {
        use crate::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let mut node_id = [0u8; 20];
        node_id[0] = 0xAB;

        let mut node_bytes = Vec::with_capacity(26);
        node_bytes.extend_from_slice(&node_id);
        node_bytes.extend_from_slice(&[10, 0, 0, 2]);
        node_bytes.extend_from_slice(&[0x13, 0x88]);

        let mut r_dict = BTreeMap::new();
        r_dict.insert(b"nodes".to_vec(), BencodeValue::Bytes(node_bytes));

        let msg = DhtMessage::new_response(vec![1, 2], BencodeValue::Dict(r_dict));
        let nodes = extract_compact_nodes_from_response(&msg);
        assert_eq!(nodes.len(), 1);
        let (addr, nid) = &nodes[0];
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2))
        );
        assert_eq!(addr.port(), 5000);
        assert_eq!(nid[0], 0xAB);
    }

    #[test]
    fn test_extract_compact_nodes_multiple() {
        use crate::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let mut n1 = vec![0xAA; 20];
        n1.extend_from_slice(&[192, 168, 1, 100, 0x1F, 0x40]);

        let mut n2 = vec![0xBB; 20];
        n2.extend_from_slice(&[172, 16, 0, 1, 0x07, 0xD0]);

        let combined: Vec<u8> = [n1.as_slice(), n2.as_slice()].concat();

        let mut r_dict = BTreeMap::new();
        r_dict.insert(b"nodes".to_vec(), BencodeValue::Bytes(combined));

        let msg = DhtMessage::new_response(vec![1, 2], BencodeValue::Dict(r_dict));
        let nodes = extract_compact_nodes_from_response(&msg);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].0.port(), 8000);
        assert_eq!(nodes[1].0.port(), 2000);
    }

    #[test]
    fn test_generate_random_node_id_nonzero() {
        let id = generate_random_node_id();
        assert!(
            !id.iter().all(|&b| b == 0),
            "Random ID should not be all zeros"
        );
    }

    #[test]
    fn test_dht_client_new_with_bootstrap() {
        let config = DhtClientConfig {
            self_id: generate_random_node_id(),
            bootstrap_nodes: vec![
                "127.0.0.1:6881".parse().unwrap(),
                "127.0.0.1:6882".parse().unwrap(),
            ],
            ..Default::default()
        };
        let client = DhtClient::new(config);
        let rt = client.routing_table();
        assert!(
            rt.total_node_count() >= 1,
            "bootstrap nodes share zero ID so only one is stored"
        );
    }

    #[test]
    fn test_discover_peers_no_bootstrap_returns_empty() {
        let config = DhtClientConfig {
            self_id: generate_random_node_id(),
            bootstrap_nodes: vec![],
            max_concurrent_queries: 1,
            query_timeout: Duration::from_millis(50),
            max_rounds: 1,
        };
        let mut client = DhtClient::new(config);
        let target = [0u8; 20];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(client.discover_peers(&target)).unwrap();
        assert!(result.addresses.is_empty());
    }

    // ==================== H1: IPv6 Compact Node Tests ====================

    #[test]
    fn test_parse_compact_nodes_ipv4() {
        // Build type-byte format data for 2 IPv4 nodes
        // Type 0x04 + IP(4) + port(2) = 7 bytes per node
        let mut data = Vec::new();

        // Node 1: 192.168.1.1:8080
        data.push(0x04); // type
        data.extend_from_slice(&[192, 168, 1, 1]); // IP
        data.extend_from_slice(&[0x1F, 0x90]); // port 8080

        // Node 2: 10.0.0.2:6881
        data.push(0x04); // type
        data.extend_from_slice(&[10, 0, 0, 2]); // IP
        data.extend_from_slice(&[0x1A, 0xE1]); // port 6881

        let nodes = parse_compact_nodes(&data, 0, 10);
        assert_eq!(nodes.len(), 2);

        match &nodes[0] {
            CompactNode::V4(ip, port) => {
                assert_eq!(*ip, std::net::Ipv4Addr::new(192, 168, 1, 1));
                assert_eq!(*port, 8080);
            }
            _ => panic!("Expected IPv4 node"),
        }

        match &nodes[1] {
            CompactNode::V4(ip, port) => {
                assert_eq!(*ip, std::net::Ipv4Addr::new(10, 0, 0, 2));
                assert_eq!(*port, 6881);
            }
            _ => panic!("Expected IPv4 node"),
        }
    }

    #[test]
    fn test_parse_compact_nodes_ipv6() {
        // Build type-byte format data for 2 IPv6 nodes
        // Type 0x06 + IP(16) + port(2) = 19 bytes per node
        let mut data = Vec::new();

        // Node 1: ::1:8080 (loopback)
        data.push(0x06); // type
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]); // IP ::1
        data.extend_from_slice(&[0x1F, 0x90]); // port 8080

        // Node 2: fe80::1:6881
        data.push(0x06); // type
        data.extend_from_slice(&[0xFE, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]); // IP
        data.extend_from_slice(&[0x1A, 0xE1]); // port 6881

        let nodes = parse_compact_nodes(&data, 0, 10);
        assert_eq!(nodes.len(), 2);

        match &nodes[0] {
            CompactNode::V6(ip, port) => {
                assert_eq!(*ip, std::net::Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1));
                assert_eq!(*port, 8080);
            }
            _ => panic!("Expected IPv6 node"),
        }

        match &nodes[1] {
            CompactNode::V6(ip, port) => {
                assert_eq!(*ip, std::net::Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1));
                assert_eq!(*port, 6881);
            }
            _ => panic!("Expected IPv6 node"),
        }
    }

    #[test]
    fn test_parse_mixed_nodes() {
        // Build mixed IPv4/IPv6 response
        let mut data = Vec::new();

        // IPv4 node first
        data.push(0x04);
        data.extend_from_slice(&[172, 16, 0, 1]);
        data.extend_from_slice(&[0x1F, 0x90]);

        // Then IPv6 node
        data.push(0x06);
        data.extend_from_slice(&[0x20, 0x01, 0x0D, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        data.extend_from_slice(&[0x1A, 0xE1]);

        // Another IPv4 node
        data.push(0x04);
        data.extend_from_slice(&[10, 0, 0, 5]);
        data.extend_from_slice(&[0x00, 0x50]);

        let nodes = parse_compact_nodes(&data, 0, 10);
        assert_eq!(nodes.len(), 3);

        // Verify types are correct
        assert!(matches!(nodes[0], CompactNode::V4(..)));
        assert!(matches!(nodes[1], CompactNode::V6(..)));
        assert!(matches!(nodes[2], CompactNode::V4(..)));
    }

    #[test]
    fn test_parse_compact_nodes_unknown_type_stops() {
        let mut data = Vec::new();

        // Valid IPv4 node
        data.push(0x04);
        data.extend_from_slice(&[192, 168, 1, 1]);
        data.extend_from_slice(&[0x1F, 0x90]);

        // Unknown type byte - should stop parsing here
        data.push(0xFF);

        // This should NOT be parsed
        data.push(0x04);
        data.extend_from_slice(&[10, 0, 0, 1]);
        data.extend_from_slice(&[0x00, 0x50]);

        let nodes = parse_compact_nodes(&data, 0, 10);
        assert_eq!(nodes.len(), 1); // Only the first valid node
    }

    #[test]
    fn test_parse_compact_nodes_truncated_data() {
        let mut data = Vec::new();
        // Type byte for IPv6 but not enough data after it
        data.push(0x06);
        data.extend_from_slice(&[0, 0, 0, 0]); // Only 4 bytes of IP instead of 16

        let nodes = parse_compact_nodes(&data, 0, 10);
        assert!(nodes.is_empty()); // Should return empty due to truncation
    }

    #[test]
    fn test_compact_node_to_socket_addr_v4() {
        let node = CompactNode::V4(std::net::Ipv4Addr::new(192, 168, 1, 100), 8080);
        let addr = node.to_socket_addr();
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 100))
        );
        assert_eq!(addr.port(), 8080);
    }

    #[test]
    fn test_compact_node_to_socket_addr_v6() {
        let node = CompactNode::V6(
            std::net::Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 1),
            443,
        );
        let addr = node.to_socket_addr();
        assert!(matches!(addr.ip(), std::net::IpAddr::V6(_)));
        assert_eq!(addr.port(), 443);
    }
}
