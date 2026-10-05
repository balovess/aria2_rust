use super::super::routing_table::RoutingTable;
use super::*;
use std::path::PathBuf;

#[test]
fn test_snapshot_freshness_rejects_old_timestamps() {
    assert!(DhtPersistence::is_fresh(
        u64::MAX,
        std::time::Duration::from_secs(60)
    ));
    assert!(!DhtPersistence::is_fresh(
        0,
        std::time::Duration::from_secs(60)
    ));
}

#[test]
fn test_serialize_header_magic_and_version() {
    let id = [0x42u8; 20];
    let data = DhtPersistence::serialize(&id, &[]);
    assert_eq!(data[0], 0xA1);
    assert_eq!(data[1], 0xA2);
    assert_eq!(data[2], 0x02);
    assert_eq!(data[7], 0x03);
}

#[test]
fn test_serialize_local_node_id() {
    let id = [0xABu8; 20];
    let data = DhtPersistence::serialize(&id, &[]);
    let stored_id: [u8; 20] = data[24..44].try_into().unwrap();
    assert_eq!(stored_id, id);
}

#[test]
fn test_serialize_nodes_ipv4() {
    let id = [0u8; 20];
    let addr: std::net::SocketAddr = "192.168.1.100:6881".parse().unwrap();
    let node = DhtNode::new(id, addr);
    let data = DhtPersistence::serialize(&id, &[node]);

    assert_eq!(data[56], 6, "IPv4 compact length should be 6");
    let ip_start = 64;
    assert_eq!(data[ip_start], 192);
    assert_eq!(data[ip_start + 1], 168);
    assert_eq!(data[ip_start + 2], 1);
    assert_eq!(data[ip_start + 3], 100);
    let port = u16::from_be_bytes([data[ip_start + 4], data[ip_start + 5]]);
    assert_eq!(port, 6881);
    assert_eq!(data.len(), 56 + NODE_ENTRY_SIZE);
}

#[test]
fn test_serialize_nodes_ipv6() {
    let id = [0u8; 20];
    let addr: std::net::SocketAddr = "[::1]:6882".parse().unwrap();
    let node = DhtNode::new(id, addr);
    let data = DhtPersistence::serialize(&id, &[node]);

    assert_eq!(data[56], 18, "IPv6 compact length should be 18");
}

#[test]
fn test_serialize_empty_routing_table() {
    let id = [0xFFu8; 20];
    let data = DhtPersistence::serialize(&id, &[]);

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
    let serialized = DhtPersistence::serialize(&id, &[node]);

    let result = DhtPersistence::deserialize(&serialized).unwrap();
    assert_eq!(result.self_id, id);
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].id, id);
    assert_eq!(result.nodes[0].addr, addr);
}

#[test]
fn test_deserialize_aria2_compatible_v2_format() {
    let self_id = [0x31u8; 20];
    let node_id = [0x71u8; 20];
    let addr: std::net::SocketAddr = "203.0.113.17:6881".parse().unwrap();
    let node = DhtNode::new(node_id, addr);
    let mut serialized = DhtPersistence::serialize(&self_id, &[node]);

    serialized[7] = 0x02;
    serialized[8..12].copy_from_slice(&1_700_000_000u32.to_be_bytes());
    serialized[12..16].fill(0);

    let restored = DhtPersistence::deserialize(&serialized)
        .expect("aria2 v2 DHT routing-table snapshots should remain readable");
    assert_eq!(restored.self_id, self_id);
    assert_eq!(restored.saved_at_secs, 1_700_000_000);
    assert_eq!(restored.nodes.len(), 1);
    assert_eq!(restored.nodes[0].id, node_id);
    assert_eq!(restored.nodes[0].addr, addr);
}

#[test]
fn test_repeated_file_save_replaces_existing_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dht.dat");
    let first = DhtNode::new([0x01; 20], "127.0.0.1:6881".parse().unwrap());
    let second = DhtNode::new([0x02; 20], "127.0.0.1:6882".parse().unwrap());
    DhtPersistence::save_to_file_sync(&path, &[0xAA; 20], &[first]).unwrap();
    DhtPersistence::save_to_file_sync(&path, &[0xBB; 20], std::slice::from_ref(&second)).unwrap();

    let restored = DhtPersistence::load_from_file_sync(&path).unwrap();
    assert_eq!(restored.self_id, [0xBB; 20]);
    assert_eq!(restored.nodes.len(), 1);
    assert_eq!(restored.nodes[0].id, second.id);
    assert_eq!(restored.nodes[0].addr, second.addr);
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

    let serialized = DhtPersistence::serialize(&self_id, &nodes);
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
    for _ in 0..5 {
        bad_node.record_failure();
    }

    rt.insert(good_node);
    rt.insert(bad_node);

    let collected = rt.collect_good_nodes();
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

    let serialized = DhtPersistence::serialize(&self_id, &nodes);
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
fn test_truncated_node_record_is_rejected() {
    let node = DhtNode::new([0x11; 20], "127.0.0.1:6881".parse().unwrap());
    let mut data = DhtPersistence::serialize(&[0x22; 20], &[node]);
    data.truncate(data.len() - 1);

    assert!(DhtPersistence::deserialize(&data).is_err());
}

#[test]
fn test_invalid_compact_peer_length_is_rejected() {
    let node = DhtNode::new([0x11; 20], "127.0.0.1:6881".parse().unwrap());
    let mut data = DhtPersistence::serialize(&[0x22; 20], &[node]);
    data[56] = 5;

    assert!(DhtPersistence::deserialize(&data).is_err());
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
