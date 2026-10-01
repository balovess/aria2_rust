use super::super::{DhtEngine, DhtEngineConfig};
use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::node::DhtNode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

#[tokio::test]
async fn test_krpc_loopback_messages_include_aria2_version() {
    let engine_id = [0x41; 20];
    let remote_id = [0x42; 20];
    let engine = DhtEngine::start(DhtEngineConfig {
        self_id: engine_id,
        listen_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    })
    .await
    .unwrap();

    let remote = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let remote_addr = remote.local_addr().unwrap();
    let add_node_engine = engine.clone();
    let add_node = tokio::spawn(async move {
        add_node_engine.add_node(remote_addr).await;
    });

    let mut packet = [0u8; 2048];
    let (length, engine_addr) =
        tokio::time::timeout(Duration::from_secs(2), remote.recv_from(&mut packet))
            .await
            .expect("engine should send a ping to the loopback node")
            .unwrap();
    assert_aria2_dht_version(&packet[..length]);
    let query = DhtMessage::decode(&packet[..length]).unwrap();
    assert!(query.is_query());
    assert_eq!(
        query.q.as_ref().map(|method| method.0.as_str()),
        Some("ping")
    );

    let response = DhtMessageBuilder::ping_response(&query.t, &remote_id).encode();
    remote.send_to(&response, engine_addr).await.unwrap();
    add_node.await.unwrap();

    let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let ping = DhtMessageBuilder::ping(0x1234_5678, &remote_id).encode();
    client.send_to(&ping, engine.local_addr()).await.unwrap();
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut packet))
        .await
        .expect("engine should answer the loopback ping")
        .unwrap();
    assert_aria2_dht_version(&packet[..length]);
    let response = DhtMessage::decode(&packet[..length]).unwrap();
    assert!(response.is_response());

    engine.shutdown_async().await;
}

fn assert_aria2_dht_version(packet: &[u8]) {
    let (message, consumed) = BencodeValue::decode(packet).unwrap();
    assert_eq!(consumed, packet.len());
    assert_eq!(
        message.dict_get(b"v").and_then(BencodeValue::as_bytes),
        Some(b"A2\x00\x03".as_slice()),
        "KRPC packet must carry the aria2 version field"
    );
}

#[tokio::test]
async fn test_bep44_udp_roundtrip_and_store_restart() {
    use crate::bittorrent::bencode::codec::BencodeValue;
    use ed25519_dalek::{Signer, SigningKey};

    let temp_dir = tempfile::tempdir().unwrap();
    let dht_path = temp_dir.path().join("dht.dat");
    let node_a_id = [0x11; 20];
    let node_b_id = [0x22; 20];
    let engine_a = DhtEngine::start(DhtEngineConfig {
        self_id: node_a_id,
        listen_addr: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    })
    .await
    .unwrap();
    let engine_b_config = DhtEngineConfig {
        self_id: node_b_id,
        dht_file_path: Some(dht_path.clone()),
        listen_addr: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    };
    let engine_b = DhtEngine::start(engine_b_config.clone()).await.unwrap();
    let b_addr = engine_b.context.task_context.socket.local_addr();
    engine_a
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::new(node_b_id, b_addr));

    let immutable = BencodeValue::Bytes(b"udp immutable".to_vec());
    assert!(engine_a.put_immutable(&immutable).await.unwrap());
    let immutable_target = StoredItem::immutable_target(&immutable);
    assert!(matches!(
        engine_a.get_item(&immutable_target, None).await.unwrap(),
        Some(StoredItem::Immutable { .. })
    ));

    let key = SigningKey::from_bytes(&[3u8; 32]);
    let mut mutable = MutableValue {
        public_key: key.verifying_key().to_bytes(),
        signature: [0; 64],
        sequence: 1,
        salt: Some(b"udp".to_vec()),
        value: BencodeValue::Bytes(b"mutable value".to_vec()),
    };
    mutable.signature = key.sign(&mutable.signed_payload()).to_bytes();
    assert!(engine_a.put_mutable(&mutable, None).await.unwrap());
    let mutable_target = StoredItem::mutable_target(&mutable.public_key, mutable.salt.as_deref());
    assert!(
        engine_a
            .get_item(&mutable_target, None)
            .await
            .unwrap()
            .is_some()
    );

    let mut updated = mutable.clone();
    updated.sequence = 2;
    updated.value = BencodeValue::Bytes(b"updated value".to_vec());
    updated.signature = key.sign(&updated.signed_payload()).to_bytes();
    assert!(!engine_a.put_mutable(&updated, Some(0)).await.unwrap());
    assert!(engine_a.put_mutable(&updated, Some(1)).await.unwrap());

    engine_b.shutdown_async().await;
    let item_path = dht_path.with_extension("items");
    assert!(item_path.exists());
    let engine_b = DhtEngine::start(engine_b_config).await.unwrap();
    let b_addr = engine_b.context.task_context.socket.local_addr();
    engine_a
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::new(node_b_id, b_addr));
    assert!(
        engine_a
            .get_item(&immutable_target, None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        engine_a
            .get_item(&mutable_target, None)
            .await
            .unwrap()
            .is_some()
    );

    engine_b.shutdown_async().await;
    engine_a.shutdown_async().await;
}

#[tokio::test]
async fn test_bep51_udp_roundtrip_uses_peer_store_samples() {
    let node_a_id = [0x31; 20];
    let node_b_id = [0x32; 20];
    let engine_a = DhtEngine::start(DhtEngineConfig {
        self_id: node_a_id,
        listen_addr: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    })
    .await
    .unwrap();
    let engine_b = DhtEngine::start(DhtEngineConfig {
        self_id: node_b_id,
        listen_addr: Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ..DhtEngineConfig::local()
    })
    .await
    .unwrap();

    let sampled_info_hash = [0xA7; 20];
    engine_b
        .context
        .peer_storage
        .add_peer(sampled_info_hash, "127.0.0.1:6881".parse().unwrap());
    let b_addr = engine_b.context.task_context.socket.local_addr();
    engine_a
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::new(node_b_id, b_addr));

    let response = engine_a
        .sample_infohashes(&[0xA6; 20])
        .await
        .unwrap()
        .expect("BEP 51 peer should return a sample response");
    assert_eq!(response.interval, 900);
    assert_eq!(response.num, 1);
    assert_eq!(response.samples, vec![sampled_info_hash]);

    engine_b.shutdown_async().await;
    engine_a.shutdown_async().await;
}

/// Read-only public-network smoke test for DNS bootstrap and KRPC lookup.
/// It is intentionally ignored because availability depends on the host
/// network and public DHT nodes; CI can opt in explicitly.
#[tokio::test]
#[ignore = "requires public DHT connectivity"]
async fn test_public_dht_find_peers_interoperability() {
    run_public_dht_find_peers_smoke(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).await;
}

#[tokio::test]
#[ignore = "requires public IPv6 DHT connectivity"]
async fn test_public_dht_find_peers_interoperability_ipv6() {
    run_public_dht_find_peers_smoke(IpAddr::V6(Ipv6Addr::UNSPECIFIED)).await;
}

async fn run_public_dht_find_peers_smoke(listen_addr: IpAddr) {
    let family = match listen_addr {
        IpAddr::V4(_) => "IPv4",
        IpAddr::V6(_) => "IPv6",
    };
    let bootstrap_nodes = std::env::var("ARIA2_TEST_DHT_BOOTSTRAP_NODES")
        .ok()
        .map(|raw| {
            let nodes = raw
                .split(',')
                .map(str::trim)
                .filter(|endpoint| !endpoint.is_empty())
                .map(|endpoint| {
                    endpoint.parse::<SocketAddr>().unwrap_or_else(|error| {
                        panic!(
                            "invalid ARIA2_TEST_DHT_BOOTSTRAP_NODES endpoint {endpoint:?}: {error}"
                        )
                    })
                })
                .filter(|endpoint| endpoint.is_ipv6() == listen_addr.is_ipv6())
                .collect::<Vec<_>>();
            assert!(
                !nodes.is_empty(),
                "ARIA2_TEST_DHT_BOOTSTRAP_NODES has no endpoints for {family}"
            );
            nodes
        })
        .unwrap_or_default();
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        listen_addr: Some(listen_addr),
        bootstrap_nodes,
        bootstrap_timeout: Duration::from_secs(15),
        ..DhtEngineConfig::default()
    })
    .await
    .unwrap_or_else(|error| {
        panic!("public {family} DHT engine should bind an ephemeral port: {error}")
    });

    engine
        .wait_until_ready(Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| {
            panic!("public {family} DHT engine did not finish bootstrap setup: {error}")
        });
    let local_addr = engine.local_addr();
    let bootstrap_stats = engine.stats().await;
    assert!(
        bootstrap_stats.total_nodes > 0,
        "public {family} DHT bootstrap resolved no nodes: local_addr={local_addr}"
    );
    let good_nodes_before_lookup = engine
        .context
        .task_context
        .routing_table
        .read()
        .await
        .collect_good_nodes();
    let bootstrap_endpoints = engine
        .context
        .task_context
        .routing_table
        .read()
        .await
        .get_all_buckets()
        .into_iter()
        .flat_map(|bucket| bucket.nodes().iter().map(DhtNode::addr))
        .collect::<Vec<_>>();
    let result = tokio::time::timeout(Duration::from_secs(30), engine.find_peers(&[0x3Cu8; 20]))
        .await
        .unwrap_or_else(|_| panic!("public {family} DHT lookup timed out"))
        .unwrap_or_else(|error| panic!("public {family} DHT lookup failed: {error}"));
    let good_nodes_after_lookup = engine
        .context
        .task_context
        .routing_table
        .read()
        .await
        .collect_good_nodes();
    let stats = engine.stats().await;
    engine.shutdown_async().await;

    let received_valid_response = good_nodes_after_lookup.iter().any(|node| {
        good_nodes_before_lookup
            .iter()
            .find(|previous| previous.id() == node.id() && previous.addr() == node.addr())
            .is_none_or(|previous| node.last_seen() > previous.last_seen())
    });
    assert!(
        received_valid_response,
        "public {family} DHT lookup observed no valid node response: local_addr={local_addr}, queries_sent={}, bootstrap_nodes={}, bootstrap_endpoints={bootstrap_endpoints:?}, bootstrap_good_nodes={}, lookup_good_nodes_before={}, lookup_good_nodes_after={}, total_nodes={}, good_nodes={}, pending_transactions={}",
        result.nodes_contacted,
        bootstrap_stats.total_nodes,
        bootstrap_stats.good_nodes,
        good_nodes_before_lookup.len(),
        good_nodes_after_lookup.len(),
        stats.total_nodes,
        stats.good_nodes,
        stats.pending_transactions,
    );
}
