use std::net::SocketAddr;
use std::time::Duration;
use tracing::debug;

use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::transaction::TransactionManager;
use crate::bittorrent::bencode::codec::BencodeValue;

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

pub struct DhtClient {
    config: DhtClientConfig,
    routing_table: RoutingTable,
    tx_manager: TransactionManager,
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
        }
    }

    pub async fn discover_peers(
        &mut self,
        target_info_hash: &[u8; 20],
    ) -> Result<DiscoveredPeers, String> {
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
                return Ok(DiscoveredPeers {
                    addresses: all_peers,
                    nodes_contacted,
                });
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

                let tx_id = (round * 1000 + i as usize) as u32;

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
            if bytes.len() >= 6 {
                let ip_bytes: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
                let port = u16::from_be_bytes([bytes[4], bytes[5]]);
                peers.push(SocketAddr::from((std::net::Ipv4Addr::from(ip_bytes), port)));
            }
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
}
