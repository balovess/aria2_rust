use super::super::{DhtEngine, DhtEngineConfig};
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::node::DhtNode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

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
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        listen_addr: Some(listen_addr),
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
