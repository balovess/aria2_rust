use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::node::DhtNode;
use super::routing_table::RoutingTable;

const DHT_MAGIC: &[u8] = &[0xA1, 0xA2];
const DHT_FORMAT_ID: u8 = 0x02;
const DHT_VERSION_3: u8 = 0x03;
const DHT_VERSION_2: u8 = 0x02;
const NODE_ENTRY_SIZE: usize = 48;

#[derive(Debug, Clone)]
pub struct PersistedNode {
    pub id: [u8; 20],
    pub addr: std::net::SocketAddr,
}

#[derive(Debug, Clone)]
pub struct DhtPersistedData {
    pub self_id: [u8; 20],
    pub saved_at_secs: u64,
    pub nodes: Vec<PersistedNode>,
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn socket_addr_to_compact(addr: &std::net::SocketAddr) -> Vec<u8> {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            let mut buf = vec![0u8; 6];
            buf[..4].copy_from_slice(&v4.ip().octets());
            buf[4..6].copy_from_slice(&v4.port().to_be_bytes());
            buf
        }
        std::net::SocketAddr::V6(v6) => {
            let mut buf = vec![0u8; 18];
            buf[..16].copy_from_slice(&v6.ip().octets());
            buf[16..18].copy_from_slice(&v6.port().to_be_bytes());
            buf
        }
    }
}

fn compact_to_socket_addr(data: &[u8]) -> Option<std::net::SocketAddr> {
    if data.len() == 6 {
        let ip = std::net::Ipv4Addr::new(data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
            ip, port,
        )))
    } else if data.len() == 18 {
        let octets: [u8; 16] = data[..16].try_into().ok()?;
        let ip = std::net::Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([data[16], data[17]]);
        Some(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
            ip, port, 0, 0,
        )))
    } else {
        None
    }
}

pub struct DhtPersistence;

impl DhtPersistence {
    pub fn serialize(self_id: &[u8; 20], nodes: &[DhtNode]) -> Result<Vec<u8>, String> {
        let mut buf = Vec::with_capacity(256 + nodes.len() * NODE_ENTRY_SIZE);

        let mut header = [0u8; 8];
        header[0] = DHT_MAGIC[0];
        header[1] = DHT_MAGIC[1];
        header[2] = DHT_FORMAT_ID;
        header[6] = 0;
        header[7] = DHT_VERSION_3;
        buf.extend_from_slice(&header);

        let timestamp = current_epoch_secs().to_be_bytes();
        buf.extend_from_slice(&timestamp);

        let reserved8 = [0u8; 8];
        buf.extend_from_slice(&reserved8);
        buf.extend_from_slice(self_id);
        let reserved4 = [0u8; 4];
        buf.extend_from_slice(&reserved4);

        let node_count = (nodes.len() as u32).to_be_bytes();
        buf.extend_from_slice(&node_count);
        buf.extend_from_slice(&reserved4);

        for node in nodes {
            let compact = socket_addr_to_compact(&node.addr);
            let clen = compact.len() as u8;

            buf.push(clen);
            let reserved7 = [0u8; 7];
            buf.extend_from_slice(&reserved7);
            buf.extend_from_slice(&compact);

            let pad_len = 24 - compact.len();
            let padding = vec![0u8; pad_len];
            buf.extend_from_slice(&padding);

            buf.extend_from_slice(&node.id);
            buf.extend_from_slice(&reserved4);
        }

        Ok(buf)
    }

    pub fn deserialize(data: &[u8]) -> Result<DhtPersistedData, String> {
        if data.len() < 56 {
            return Err("dht.dat 数据太短".into());
        }

        let header_v3: [u8; 8] = [
            DHT_MAGIC[0],
            DHT_MAGIC[1],
            DHT_FORMAT_ID,
            0,
            0,
            0,
            0,
            DHT_VERSION_3,
        ];
        let header_v2: [u8; 8] = [
            DHT_MAGIC[0],
            DHT_MAGIC[1],
            DHT_FORMAT_ID,
            0,
            0,
            0,
            0,
            DHT_VERSION_2,
        ];

        let version = if data[..8] == header_v3[..] {
            3
        } else if data[..8] == header_v2[..] {
            2
        } else {
            return Err(format!("dht.dat 无效的 magic/version: {:02x?}", &data[..8]));
        };

        let mut offset = 8;

        let saved_at_secs = if version >= 3 {
            if offset + 8 > data.len() {
                return Err("dht.dat 时间戳截断".into());
            }
            let ts = u64::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            offset += 8;
            ts
        } else {
            if offset + 8 > data.len() {
                return Err("dht.dat 时间戳截断 (v2)".into());
            }
            let ts32 = u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as u64;
            offset += 8;
            ts32
        };

        if offset + 32 > data.len() {
            return Err("dht.dat localnode 截断".into());
        }
        offset += 8;
        let self_id: [u8; 20] = data[offset..offset + 20]
            .try_into()
            .map_err(|_| "dht.dat self_id 长度错误")?;
        offset += 20;
        offset += 4;

        if offset + 8 > data.len() {
            return Err("dht.dat 节点计数截断".into());
        }
        let num_nodes = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 8;

        let expected_end = offset + num_nodes * NODE_ENTRY_SIZE;
        if expected_end > data.len() {
            return Err(format!(
                "dht.dat 节点数据截断: 需要 {} 字节，实际 {}",
                expected_end,
                data.len()
            ));
        }

        let mut nodes = Vec::with_capacity(num_nodes);
        for _ in 0..num_nodes {
            let clen = data[offset] as usize;
            offset += 1;
            offset += 7;

            if clen != 6 && clen != 18 {
                offset += NODE_ENTRY_SIZE - 8;
                continue;
            }

            if offset + clen > data.len() {
                break;
            }
            let compact = &data[offset..offset + clen];

            if compact.iter().all(|&b| b == 0) {
                offset += NODE_ENTRY_SIZE - 8;
                continue;
            }

            let addr = match compact_to_socket_addr(compact) {
                Some(a) => a,
                None => {
                    offset += NODE_ENTRY_SIZE - 8;
                    continue;
                }
            };
            offset += clen;

            let pad_remaining = 24 - clen;
            offset += pad_remaining;

            if offset + 20 > data.len() {
                break;
            }
            let id: [u8; 20] = data[offset..offset + 20]
                .try_into()
                .map_err(|_| "dht.dat 节点 ID 长度错误")?;
            offset += 20;
            offset += 4;

            nodes.push(PersistedNode { id, addr });
        }

        Ok(DhtPersistedData {
            self_id,
            saved_at_secs,
            nodes,
        })
    }

    pub fn collect_good_nodes(rt: &RoutingTable) -> Vec<DhtNode> {
        let mut result = Vec::new();
        for bucket_idx in 0..160 {
            if let Some(bucket) = rt.get_bucket(bucket_idx) {
                for node in bucket.get_nodes() {
                    if node.is_good() {
                        result.push(node.clone());
                    }
                }
            }
        }
        result
    }

    pub async fn save_to_file(
        path: &Path,
        self_id: &[u8; 20],
        nodes: &[DhtNode],
    ) -> Result<usize, String> {
        let data = Self::serialize(self_id, nodes)?;

        let tmp_path = path.with_extension("dat.tmp");
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| format!("写入临时文件失败 {}: {}", tmp_path.display(), e))?;

        tokio::fs::rename(&tmp_path, path).await.map_err(|e| {
            format!(
                "重命名失败 {} -> {}: {}",
                tmp_path.display(),
                path.display(),
                e
            )
        })?;

        Ok(nodes.len())
    }

    pub fn save_to_file_sync(
        path: &Path,
        self_id: &[u8; 20],
        nodes: &[DhtNode],
    ) -> Result<usize, String> {
        let data = Self::serialize(self_id, nodes)?;

        let tmp_path = path.with_extension("dat.tmp");
        std::fs::write(&tmp_path, &data)
            .map_err(|e| format!("写入临时文件失败 {}: {}", tmp_path.display(), e))?;

        std::fs::rename(&tmp_path, path).map_err(|e| {
            format!(
                "重命名失败 {} -> {}: {}",
                tmp_path.display(),
                path.display(),
                e
            )
        })?;

        Ok(nodes.len())
    }

    pub async fn load_from_file(path: &Path) -> Result<DhtPersistedData, String> {
        let data = tokio::fs::read(path)
            .await
            .map_err(|e| format!("读取 dht.dat 失败 {}: {}", path.display(), e))?;
        Self::deserialize(&data)
    }

    pub fn load_from_file_sync(path: &Path) -> Result<DhtPersistedData, String> {
        let data = std::fs::read(path)
            .map_err(|e| format!("读取 dht.dat 失败 {}: {}", path.display(), e))?;
        Self::deserialize(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_serialize_header_magic_and_version() {
        let id = [0x42u8; 20];
        let data = DhtPersistence::serialize(&id, &[]).unwrap();
        assert_eq!(data[0], 0xA1);
        assert_eq!(data[1], 0xA2);
        assert_eq!(data[2], 0x02);
        assert_eq!(data[7], 0x03);
    }

    #[test]
    fn test_serialize_local_node_id() {
        let id = [0xABu8; 20];
        let data = DhtPersistence::serialize(&id, &[]).unwrap();
        let stored_id: [u8; 20] = data[24..44].try_into().unwrap();
        assert_eq!(stored_id, id);
    }

    #[test]
    fn test_serialize_nodes_ipv4() {
        let id = [0u8; 20];
        let addr: std::net::SocketAddr = "192.168.1.100:6881".parse().unwrap();
        let node = DhtNode::new(id, addr);
        let data = DhtPersistence::serialize(&id, &[node]).unwrap();

        assert_eq!(data[56], 6, "IPv4 compact length should be 6");
        let ip_start = 64;
        assert_eq!(data[ip_start], 192);
        assert_eq!(data[ip_start + 1], 168);
        assert_eq!(data[ip_start + 2], 1);
        assert_eq!(data[ip_start + 3], 100);
        let port = u16::from_be_bytes([data[ip_start + 4], data[ip_start + 5]]);
        assert_eq!(port, 6881);
    }

    #[test]
    fn test_serialize_nodes_ipv6() {
        let id = [0u8; 20];
        let addr: std::net::SocketAddr = "[::1]:6882".parse().unwrap();
        let node = DhtNode::new(id, addr);
        let data = DhtPersistence::serialize(&id, &[node]).unwrap();

        assert_eq!(data[56], 18, "IPv6 compact length should be 18");
    }

    #[test]
    fn test_serialize_empty_routing_table() {
        let id = [0xFFu8; 20];
        let data = DhtPersistence::serialize(&id, &[]).unwrap();

        let num_nodes = u32::from_be_bytes([data[44], data[45], data[46], data[47]]);
        assert_eq!(num_nodes, 0);
        assert_eq!(
            data.len(),
            56,
            "empty table should be exactly 56 bytes (header+ts+localnode+count)"
        );
    }

    #[test]
    fn test_deserialize_v3_format() {
        let id = [0x11u8; 20];
        let addr: std::net::SocketAddr = "10.0.0.5:6881".parse().unwrap();
        let node = DhtNode::new(id, addr);
        let serialized = DhtPersistence::serialize(&id, &[node]).unwrap();

        let result = DhtPersistence::deserialize(&serialized).unwrap();
        assert_eq!(result.self_id, id);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].id, id);
        assert_eq!(result.nodes[0].addr, addr);
    }

    #[test]
    fn test_deserialize_v2_compat() {
        let mut data = vec![0u8; 56];
        data[0] = 0xA1;
        data[1] = 0xA2;
        data[2] = 0x02;
        data[7] = 0x02;
        data[8..12].copy_from_slice(&(1000u32).to_be_bytes());

        let id = [0xCCu8; 20];
        data[24..44].copy_from_slice(&id);

        let result = DhtPersistence::deserialize(&data).unwrap();
        assert_eq!(result.self_id, id);
        assert_eq!(result.saved_at_secs, 1000);
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn test_roundtrip_serialize_deserialize() {
        let self_id = [0xDEu8; 20];
        let addrs: Vec<std::net::SocketAddr> = vec![
            "1.2.3.4:6881".parse().unwrap(),
            "[2001:db8::1]:6882".parse().unwrap(),
            "10.0.0.1:6883".parse().unwrap(),
        ];
        let nodes: Vec<DhtNode> = addrs
            .iter()
            .enumerate()
            .map(|(i, a)| DhtNode::new([i as u8; 20], *a))
            .collect();

        let serialized = DhtPersistence::serialize(&self_id, &nodes).unwrap();
        let deserialized = DhtPersistence::deserialize(&serialized).unwrap();

        assert_eq!(deserialized.self_id, self_id);
        assert_eq!(deserialized.nodes.len(), 3);
        for (i, node) in deserialized.nodes.iter().enumerate().take(3) {
            assert_eq!(node.addr, addrs[i]);
        }
    }

    #[test]
    fn test_reject_bad_header() {
        let bad_data = vec![0x00u8; 16];
        let result = DhtPersistence::deserialize(&bad_data);
        assert!(result.is_err(), "bad magic should fail");
    }

    #[test]
    fn test_collect_good_nodes_only() {
        let mut rt = RoutingTable::new([0x80u8; 20]);

        let good_addr = "127.0.0.1:6881".parse().unwrap();
        let good_node = DhtNode::new([1u8; 20], good_addr);

        let bad_addr = "127.0.0.1:6882".parse().unwrap();
        let mut bad_node = DhtNode::new([2u8; 20], bad_addr);
        for _ in 0..3 {
            bad_node.record_failure();
        }

        rt.insert(good_node);
        rt.insert(bad_node);

        let collected = DhtPersistence::collect_good_nodes(&rt);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].addr, good_addr);
    }

    #[test]
    fn test_save_load_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dht.dat");

        let self_id = [0x99u8; 20];
        let addr = "172.16.0.1:9999".parse().unwrap();
        let node = DhtNode::new([0xAAu8; 20], addr);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            DhtPersistence::save_to_file(&path, &self_id, &[node])
                .await
                .unwrap();

            let loaded = DhtPersistence::load_from_file(&path).await.unwrap();
            assert_eq!(loaded.self_id, self_id);
            assert_eq!(loaded.nodes.len(), 1);
            assert_eq!(loaded.nodes[0].id, [0xAAu8; 20]);
            assert_eq!(loaded.nodes[0].addr, addr);
        });
    }

    #[test]
    fn test_multiple_nodes_roundtrip() {
        let self_id = [0x12u8; 20];
        let mut nodes = Vec::new();
        for i in 0u8..20 {
            let octets = [192, 0, 1, i + 1];
            let addr = std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]),
                6881 + i as u16,
            ));
            nodes.push(DhtNode::new([i; 20], addr));
        }

        let serialized = DhtPersistence::serialize(&self_id, &nodes).unwrap();
        let deserialized = DhtPersistence::deserialize(&serialized).unwrap();
        assert_eq!(deserialized.nodes.len(), 20);
    }

    #[test]
    fn test_truncated_data_error() {
        let short_data = vec![0xA1, 0xA2];
        let result = DhtPersistence::deserialize(&short_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_socket_addr_to_compact_ipv4() {
        let addr: std::net::SocketAddr = "8.8.8.8:53".parse().unwrap();
        let compact = socket_addr_to_compact(&addr);
        assert_eq!(compact.len(), 6);
        assert_eq!(compact[0], 8);
        assert_eq!(compact[1], 8);
        assert_eq!(compact[2], 8);
        assert_eq!(compact[3], 8);
        let port = u16::from_be_bytes([compact[4], compact[5]]);
        assert_eq!(port, 53);
    }

    #[test]
    fn test_compact_to_socket_addr_ipv4() {
        let compact: Vec<u8> = vec![127, 0, 0, 1, 0x1A, 0x0B];
        let addr = compact_to_socket_addr(&compact).unwrap();
        assert_eq!(
            addr,
            "127.0.0.1:6667".parse::<std::net::SocketAddr>().unwrap()
        );
    }

    #[test]
    fn test_load_nonexistent_file_error() {
        let path = PathBuf::from("/nonexistent/path/dht.dat");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async { DhtPersistence::load_from_file(&path).await });
        assert!(result.is_err());
    }
}
