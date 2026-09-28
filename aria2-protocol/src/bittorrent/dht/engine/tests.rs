use super::{DhtEngine, DhtEngineConfig, DhtEngineState};
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::node::DhtNode;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

#[test]
fn test_dht_engine_config_default() {
    let config = DhtEngineConfig::default();
    assert_eq!(config.refresh_check_interval, Duration::from_secs(300));
    assert_eq!(config.max_concurrent_lookups, 16);
    assert_eq!(config.port, 6881);
}

#[test]
fn test_dht_engine_state_ordering() {
    assert!(DhtEngineState::Stopped < DhtEngineState::Bootstrapping);
    assert!(DhtEngineState::Bootstrapping < DhtEngineState::Running);
    assert!(DhtEngineState::Running < DhtEngineState::ShuttingDown);
}

#[tokio::test]
async fn test_dht_engine_start_shutdown() {
    // `local()` disables public-network bootstrap so the engine reaches
    // `Running` deterministically without any outbound traffic.
    let config = DhtEngineConfig {
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };

    let engine = DhtEngine::start(config)
        .await
        .expect("start should succeed");
    assert_eq!(engine.state().await, DhtEngineState::Running);
    engine
        .wait_until_ready(Duration::from_secs(1))
        .await
        .expect("running engine should already be ready");

    engine.shutdown_async().await;
    assert_eq!(engine.state().await, DhtEngineState::ShuttingDown);
    assert_eq!(engine.stats().await.state, DhtEngineState::ShuttingDown);
    assert!(
        engine
            .background_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );
}

#[tokio::test]
async fn test_dht_engine_binds_configured_ipv6_udp_source() {
    let engine = DhtEngine::start(DhtEngineConfig {
        dht_file_path: None,
        listen_addr: Some("::1".parse().expect("parse IPv6 loopback")),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should bind the configured IPv6 source");

    assert_eq!(
        engine.context.socket.local_addr().ip(),
        "::1".parse::<IpAddr>().unwrap()
    );

    engine.shutdown_async().await;
}

#[tokio::test]
async fn save_state_does_not_restore_a_node_removed_from_the_live_routing_table() {
    use crate::bittorrent::dht::persistence::DhtPersistence;

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("dht.dat");
    let stale_id = [0xA7; 20];
    let stale_node = DhtNode::new(stale_id, "127.0.0.1:6881".parse().unwrap());
    DhtPersistence::save_to_file_sync(&path, &[0xA8; 20], &[stale_node])
        .expect("seed a persisted routing-table snapshot");

    let engine = DhtEngine::start(DhtEngineConfig {
        dht_file_path: Some(path.clone()),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("local DHT engine should load the snapshot");
    assert!(
        engine.context.routing_table.write().await.remove(&stale_id),
        "the test node should be present in the live routing table"
    );

    engine
        .save_state()
        .await
        .expect("saving the live routing table should succeed");
    let saved = DhtPersistence::load_from_file_sync(&path)
        .expect("saved routing-table snapshot should remain readable");
    assert!(
        saved.nodes.is_empty(),
        "a node removed from the live routing table must not be merged back from disk"
    );

    engine.shutdown_async().await;
}

#[tokio::test]
async fn configured_bootstrap_endpoints_are_all_seeded() {
    use crate::bittorrent::dht::persistence::DhtPersistence;

    let temp_dir = tempfile::tempdir().unwrap();
    let dht_file_path = temp_dir.path().join("dht.dat");
    let bootstrap_nodes = [
        "192.0.2.1:6881",
        "192.0.2.2:6881",
        "192.0.2.3:6881",
        "192.0.2.4:6881",
        "192.0.2.5:6881",
    ]
    .into_iter()
    .map(|addr| addr.parse().unwrap())
    .collect::<Vec<_>>();
    let expected_count = bootstrap_nodes.len();
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        bootstrap_nodes,
        dht_file_path: Some(dht_file_path.clone()),
        query_timeout: Duration::from_secs(30),
        bootstrap_timeout: Duration::from_secs(5),
        ..DhtEngineConfig::default()
    })
    .await
    .expect("engine should bind and seed configured bootstrap endpoints");

    engine
        .wait_until_ready(Duration::from_secs(5))
        .await
        .expect("configured bootstrap setup should finish");
    let stats = engine.stats().await;

    assert_eq!(
        stats.total_nodes, expected_count,
        "every configured endpoint must be represented as a distinct routing-table node"
    );
    assert_eq!(
        stats.good_nodes, 0,
        "unverified bootstrap endpoints must not be treated as live good nodes"
    );
    engine
        .save_state()
        .await
        .expect("saving an unverified routing table should succeed");
    let saved = DhtPersistence::load_from_file_sync(&dht_file_path)
        .expect("saved DHT snapshot should be readable");
    assert!(
        saved.nodes.is_empty(),
        "bootstrap endpoints must not be persisted as live good nodes before responding"
    );
    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_state_subscription_is_event_driven() {
    let engine = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("local DHT engine should start");
    let mut states = engine.subscribe_state();

    assert_eq!(*states.borrow(), DhtEngineState::Running);

    engine.shutdown();
    states
        .changed()
        .await
        .expect("state subscription should receive shutdown");
    assert_eq!(*states.borrow(), DhtEngineState::ShuttingDown);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_wait_until_ready_observes_bootstrap_transition() {
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        bootstrap_nodes: vec!["127.0.0.1:9".parse().unwrap()],
        bootstrap_timeout: Duration::from_secs(1),
        ..DhtEngineConfig::default()
    })
    .await
    .expect("DHT engine should bind an ephemeral port");

    engine
        .wait_until_ready(Duration::from_secs(1))
        .await
        .expect("bootstrap lifecycle transition should be observable");
    assert_eq!(engine.state().await, DhtEngineState::Running);

    engine.shutdown_async().await;
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
    let b_addr = engine_b.context.socket.local_addr();
    engine_a
        .context
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
    let b_addr = engine_b.context.socket.local_addr();
    engine_a
        .context
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
    let b_addr = engine_b.context.socket.local_addr();
    engine_a
        .context
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
    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        query_timeout: Duration::from_secs(3),
        bootstrap_timeout: Duration::from_secs(15),
        ..DhtEngineConfig::default()
    })
    .await
    .expect("public DHT engine should bind an ephemeral port");

    engine
        .wait_until_ready(Duration::from_secs(30))
        .await
        .expect("public DHT engine did not finish bootstrap setup");
    let local_addr = engine.local_addr();
    let bootstrap_stats = engine.stats().await;
    assert!(
        bootstrap_stats.total_nodes > 0,
        "public DHT bootstrap resolved no nodes for the bound address family: local_addr={local_addr}"
    );
    let result = tokio::time::timeout(Duration::from_secs(30), engine.find_peers(&[0x3Cu8; 20]))
        .await
        .expect("public DHT lookup timed out")
        .expect("public DHT lookup failed");
    let stats = engine.stats().await;
    engine.shutdown_async().await;

    assert!(
        result.nodes_contacted > 0,
        "public DHT lookup received no node responses: local_addr={local_addr}, bootstrap_nodes={}, bootstrap_good_nodes={}, total_nodes={}, good_nodes={}, pending_transactions={}",
        bootstrap_stats.total_nodes,
        bootstrap_stats.good_nodes,
        stats.total_nodes,
        stats.good_nodes,
        stats.pending_transactions,
    );
}

#[tokio::test]
async fn test_dht_engine_sync_shutdown_is_immediately_observable() {
    let engine = DhtEngine::start(DhtEngineConfig::local())
        .await
        .expect("start should succeed");

    engine.shutdown();

    assert_eq!(engine.state().await, DhtEngineState::ShuttingDown);
    assert_eq!(engine.stats().await.state, DhtEngineState::ShuttingDown);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_tries_next_port_when_first_is_occupied() {
    let occupied = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("occupied UDP socket");
    let first_port = occupied.local_addr().unwrap().port();
    let candidate = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("candidate UDP socket");
    let second_port = candidate.local_addr().unwrap().port();
    drop(candidate);

    let config = DhtEngineConfig {
        port: first_port,
        port_range: Some(vec![first_port, second_port]),
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };
    let engine = DhtEngine::start(config)
        .await
        .expect("DHT should fall back to the next available port");

    assert_eq!(engine.context.socket.local_addr().port(), second_port);
    drop(occupied);
    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_dht_engine_stats() {
    let config = DhtEngineConfig {
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };

    let engine = DhtEngine::start(config)
        .await
        .expect("start should succeed");
    let stats = engine.stats().await;
    assert_eq!(stats.state, DhtEngineState::Running);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn test_find_peers_when_stopped() {
    // Create an engine in ShuttingDown state
    let config = DhtEngineConfig {
        dht_file_path: None,
        ..DhtEngineConfig::local()
    };

    let engine = DhtEngine::start(config)
        .await
        .expect("start should succeed");
    engine.shutdown();

    // find_peers should return empty when shutting down
    // (may take a moment for state to propagate)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = engine.find_peers(&[0u8; 20]).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn get_peers_replaces_a_node_whose_id_changed_at_the_same_endpoint() {
    const INFO_HASH: [u8; 20] = [0x34; 20];
    let old_node_id = [0xA5; 20];
    let new_node_id = [0xB6; 20];
    let latest_node_id = [0xC7; 20];
    let peer: std::net::SocketAddr = "127.0.0.1:6882".parse().unwrap();
    let responder = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("local UDP responder should bind"),
    );
    let responder_addr = responder.local_addr().expect("responder address");
    let responder_task = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut get_peers_responses = 0;
        while get_peers_responses < 2 {
            let (len, from) =
                tokio::time::timeout(Duration::from_secs(2), responder.recv_from(&mut buf))
                    .await
                    .expect("engine should send its DHT queries")
                    .expect("local responder should receive UDP packet");
            let query = DhtMessage::decode(&buf[..len]).expect("valid KRPC query");
            assert!(query.is_query());
            let response = match query.q.as_ref().map(|method| method.0.as_str()) {
                Some("ping") => DhtMessageBuilder::ping_response(&query.t, &old_node_id),
                Some("find_node") => {
                    DhtMessageBuilder::find_node_response(&query.t, &latest_node_id, &[])
                }
                Some("get_peers") => {
                    let args = query.a.as_ref().expect("get_peers query arguments");
                    assert_eq!(
                        args.dict_get(b"info_hash").and_then(|v| v.as_bytes()),
                        Some(&INFO_HASH[..])
                    );
                    let response_node_id = if get_peers_responses == 0 {
                        new_node_id
                    } else {
                        latest_node_id
                    };
                    get_peers_responses += 1;
                    DhtMessageBuilder::get_peers_response_with_peers(
                        &query.t,
                        &response_node_id,
                        b"fixture-token",
                        &[peer],
                    )
                }
                method => panic!("unexpected DHT query: {method:?}"),
            };
            let encoded = response.encode().expect("response should encode");
            responder
                .send_to(&encoded, from)
                .await
                .expect("send DHT response");
        }
    });

    let engine = DhtEngine::start(DhtEngineConfig {
        query_timeout: Duration::from_millis(500),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should start");
    engine.add_node(responder_addr).await;
    let result = engine
        .find_peers(&INFO_HASH)
        .await
        .expect("get_peers lookup should complete");
    assert_eq!(result.peers, vec![peer]);
    assert_eq!(result.nodes_contacted, 1);

    let lookup = crate::bittorrent::dht::lookup::iterative_get_peers(
        &INFO_HASH,
        &engine.context.handler_self_id,
        &engine.context.routing_table,
        &engine.context.socket,
        &engine.context.tracker,
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(lookup.peers, vec![peer]);
    assert_eq!(lookup.nodes_contacted, 1);
    assert_eq!(lookup.token_nodes.len(), 1);
    assert_eq!(lookup.token_nodes[0].0, responder_addr);
    assert_eq!(lookup.token_nodes[0].1, latest_node_id);
    assert_eq!(lookup.token_nodes[0].2, b"fixture-token");

    let routing_table = engine.context.routing_table.read().await;
    let nodes = routing_table
        .get_all_buckets()
        .into_iter()
        .flat_map(|bucket| bucket.nodes().iter())
        .collect::<Vec<_>>();
    assert!(
        nodes.iter().all(|node| node.id() != &old_node_id),
        "the superseded node ID must be removed after a valid response from the same endpoint"
    );
    assert!(
        nodes.iter().all(|node| node.id() != &new_node_id),
        "an intermediate node ID must be replaced when the endpoint changes identity again"
    );
    assert!(
        nodes
            .iter()
            .any(|node| node.id() == &latest_node_id && node.is_good()),
        "the responding node ID must replace the stale identity as a live good node"
    );
    drop(routing_table);

    engine.shutdown_async().await;
    responder_task.await.expect("responder task should finish");
}

#[tokio::test]
async fn peer_lookup_can_announce_with_the_token_from_that_same_lookup() {
    const INFO_HASH: [u8; 20] = [0x39; 20];
    const ANNOUNCE_PORT: u16 = 51413;
    let node_id = [0xD7; 20];
    let peer: SocketAddr = "127.0.0.1:6882".parse().unwrap();
    let responder = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("local UDP responder should bind"),
    );
    let responder_addr = responder.local_addr().expect("responder address");
    let responder_task = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut get_peers_count = 0;
        loop {
            let (len, from) =
                tokio::time::timeout(Duration::from_secs(2), responder.recv_from(&mut buf))
                    .await
                    .expect("combined peer lookup should finish its wire exchange")
                    .expect("local responder should receive UDP packet");
            let query = DhtMessage::decode(&buf[..len]).expect("valid KRPC query");
            assert!(query.is_query());
            let (response, announced) = match query.q.as_ref().map(|method| method.0.as_str()) {
                Some("ping") => (DhtMessageBuilder::ping_response(&query.t, &node_id), false),
                Some("find_node") => (
                    DhtMessageBuilder::find_node_response(&query.t, &node_id, &[]),
                    false,
                ),
                Some("get_peers") => {
                    get_peers_count += 1;
                    let args = query.a.as_ref().expect("get_peers query arguments");
                    assert_eq!(
                        args.dict_get(b"info_hash")
                            .and_then(|value| value.as_bytes()),
                        Some(&INFO_HASH[..])
                    );
                    (
                        DhtMessageBuilder::get_peers_response_with_peers(
                            &query.t,
                            &node_id,
                            b"fixture-token",
                            &[peer],
                        ),
                        false,
                    )
                }
                Some("announce_peer") => {
                    assert_eq!(get_peers_count, 1, "announce should reuse one lookup");
                    let args = query.a.as_ref().expect("announce_peer query arguments");
                    assert_eq!(
                        args.dict_get(b"info_hash")
                            .and_then(|value| value.as_bytes()),
                        Some(&INFO_HASH[..])
                    );
                    assert_eq!(
                        args.dict_get(b"token").and_then(|value| value.as_bytes()),
                        Some(&b"fixture-token"[..])
                    );
                    assert_eq!(
                        args.dict_get(b"port").and_then(|value| value.as_int()),
                        Some(i64::from(ANNOUNCE_PORT))
                    );
                    (
                        DhtMessageBuilder::announce_peer_response(&query.t, &node_id),
                        true,
                    )
                }
                method => panic!("unexpected DHT query: {method:?}"),
            };
            let encoded = response.encode().expect("response should encode");
            responder
                .send_to(&encoded, from)
                .await
                .expect("send DHT response");
            if announced {
                break;
            }
        }
    });

    let engine = DhtEngine::start(DhtEngineConfig {
        query_timeout: Duration::from_millis(500),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should start");
    engine.add_node(responder_addr).await;
    let result = engine
        .find_peers_and_announce(&INFO_HASH, ANNOUNCE_PORT)
        .await
        .expect("combined DHT lookup and announce should succeed");

    assert_eq!(result.peers, vec![peer]);
    assert_eq!(result.nodes_contacted, 1);
    responder_task.await.expect("responder task should finish");
    engine.shutdown_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_real_udp_dht_pressure_across_concurrency_levels() {
    const REQUESTS: usize = 64;
    const RESPONDER_WORKERS: usize = 8;
    const RESPONDER_DELAY: Duration = Duration::from_millis(2);
    const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

    let responder = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("local UDP responder should bind"),
    );
    let responder_addr = responder.local_addr().expect("responder address");
    let responder_id = [0xA5; 20];
    let responder_requests: Arc<Vec<AtomicUsize>> =
        Arc::new((0..=16).map(|_| AtomicUsize::new(0)).collect());
    let responder_responses: Arc<Vec<AtomicUsize>> =
        Arc::new((0..=16).map(|_| AtomicUsize::new(0)).collect());
    let responder_stop = CancellationToken::new();
    let mut responder_workers = tokio::task::JoinSet::new();

    for _ in 0..RESPONDER_WORKERS {
        let responder = Arc::clone(&responder);
        let responder_stop = responder_stop.clone();
        let responder_requests = Arc::clone(&responder_requests);
        let responder_responses = Arc::clone(&responder_responses);
        responder_workers.spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                let (len, from) = tokio::select! {
                    _ = responder_stop.cancelled() => break,
                    result = responder.recv_from(&mut buf) => {
                        let Ok(packet) = result else { break };
                        packet
                    }
                };
                let Ok(query) = DhtMessage::decode(&buf[..len]) else {
                    continue;
                };
                if !query.is_query() {
                    continue;
                }
                let query_level = if query
                    .q
                    .as_ref()
                    .is_some_and(|method| method.0 == "get_peers")
                {
                    query
                        .a
                        .as_ref()
                        .and_then(|args| args.dict_get(b"info_hash"))
                        .and_then(|value| value.as_bytes())
                        .and_then(|info_hash| info_hash.first().copied())
                        .filter(|level| (1..=16).contains(level))
                        .map(usize::from)
                } else {
                    None
                };
                if let Some(level) = query_level {
                    responder_requests[level].fetch_add(1, Ordering::Relaxed);
                }
                tokio::select! {
                    _ = responder_stop.cancelled() => break,
                    _ = tokio::time::sleep(RESPONDER_DELAY) => {}
                }

                let response = match query.q.as_ref().map(|method| method.0.as_str()) {
                    Some("ping") => DhtMessageBuilder::ping_response(&query.t, &responder_id),
                    Some("get_peers") => DhtMessageBuilder::get_peers_response_with_peers(
                        &query.t,
                        &responder_id,
                        b"local-token",
                        &[],
                    ),
                    _ => continue,
                };
                let Ok(encoded) = response.encode() else {
                    continue;
                };
                if responder.send_to(&encoded, from).await.is_ok()
                    && let Some(level) = query_level
                {
                    responder_responses[level].fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    let responder_task =
        tokio::spawn(async move { while responder_workers.join_next().await.is_some() {} });

    for max_concurrent_lookups in [1, 2, 4, 8, 16] {
        let config = DhtEngineConfig {
            query_timeout: QUERY_TIMEOUT,
            max_concurrent_lookups,
            ..DhtEngineConfig::local()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("DHT engine should start");
        engine.add_node(responder_addr).await;

        let node_ready = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if engine.stats().await.good_nodes > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(node_ready.is_ok(), "local responder was not added");

        let start_barrier = Arc::new(Barrier::new(REQUESTS + 1));

        let mut tasks = Vec::with_capacity(REQUESTS);
        for _ in 0..REQUESTS {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&start_barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let started = std::time::Instant::now();
                let result = engine.find_peers(&[max_concurrent_lookups as u8; 20]).await;
                (
                    started.elapsed(),
                    result.map(|result| result.nodes_contacted > 0),
                )
            }));
        }

        start_barrier.wait().await;
        let started_at = std::time::Instant::now();
        let mut latencies = Vec::with_capacity(REQUESTS);
        let mut successful = 0usize;
        for task in tasks {
            let (latency, result) = task.await.expect("lookup task should join");
            latencies.push(latency);
            if result.expect("lookup should not be cancelled") {
                successful += 1;
            }
        }
        let elapsed = started_at.elapsed();

        latencies.sort_unstable();
        let p50 = latencies[REQUESTS / 2];
        let p95 = latencies[(REQUESTS * 95 / 100).min(REQUESTS - 1)];
        let max = latencies[REQUESTS - 1];
        let received = responder_requests[max_concurrent_lookups].load(Ordering::Relaxed);
        let responses = responder_responses[max_concurrent_lookups].load(Ordering::Relaxed);
        let expected_packets = REQUESTS;
        let request_loss = expected_packets.saturating_sub(received);
        let response_loss = expected_packets.saturating_sub(responses);
        let throughput = successful as f64 / elapsed.as_secs_f64();

        println!(
            "DHT UDP pressure: concurrency={max_concurrent_lookups}, requests={REQUESTS}, successful={successful}, responder_requests={received}, responder_responses={responses}, request_loss={:.2}%, response_loss={:.2}%, p50={:?}, p95={:?}, max={:?}, throughput={throughput:.1}/s, immediate_queue_peak={}",
            request_loss as f64 * 100.0 / expected_packets as f64,
            response_loss as f64 * 100.0 / expected_packets as f64,
            p50,
            p95,
            max,
            engine
                .task_queue
                .immediate_executor()
                .peak_queue_size()
                .await,
        );

        assert_eq!(successful, REQUESTS, "local UDP pressure test lost lookups");
        assert!(
            received >= expected_packets,
            "local UDP responder received fewer packets than expected"
        );
        assert!(
            responses >= expected_packets,
            "local UDP responder sent fewer responses than expected"
        );
        engine.shutdown_async().await;
    }

    responder_stop.cancel();
    responder_task.await.expect("responder task should join");
}
