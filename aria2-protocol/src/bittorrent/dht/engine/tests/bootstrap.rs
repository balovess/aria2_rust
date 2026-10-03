use super::super::{DhtEngine, DhtEngineConfig, DhtEngineState};
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use crate::bittorrent::dht::node::DhtNode;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
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
    assert!(engine.background_tasks.lock().await.is_empty());
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
        engine.context.task_context.socket.local_addr().ip(),
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
        engine
            .context
            .task_context
            .routing_table
            .write()
            .await
            .remove(&stale_id),
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
async fn startup_restores_a_fresh_aria2_v2_routing_snapshot() {
    use crate::bittorrent::dht::persistence::DhtPersistence;
    use std::time::{SystemTime, UNIX_EPOCH};

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("dht.dat");
    let self_id = [0xA8; 20];
    let node_id = [0x27; 20];
    let addr = "127.0.0.1:6881".parse().unwrap();
    let node = DhtNode::new(node_id, addr);
    let mut snapshot = DhtPersistence::serialize(&self_id, &[node]);
    let saved_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    snapshot[7] = 0x02;
    snapshot[8..12].copy_from_slice(&saved_at_secs.to_be_bytes());
    snapshot[12..16].fill(0);
    std::fs::write(&path, snapshot).unwrap();

    let engine = DhtEngine::start(DhtEngineConfig {
        dht_file_path: Some(path),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("fresh aria2 v2 routing snapshots should load at engine startup");
    let routing_table = engine.context.task_context.routing_table.read().await;
    assert_eq!(routing_table.total_node_count(), 1);
    assert_eq!(routing_table.find_closest(&node_id, 1)[0].addr(), addr);
    assert_eq!(routing_table.find_closest(&node_id, 1)[0].id(), &node_id);
    drop(routing_table);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn startup_restores_only_nodes_matching_the_bound_ip_family() {
    use crate::bittorrent::dht::persistence::DhtPersistence;

    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("shared-dht.dat");
    let self_id = [0xA8; 20];
    let ipv4_id = [0x11; 20];
    let ipv6_id = [0x22; 20];
    let ipv4_addr = "192.0.2.11:6881".parse().unwrap();
    let ipv6_addr = "[2001:db8::22]:6881".parse().unwrap();
    let nodes = [
        DhtNode::new(ipv4_id, ipv4_addr),
        DhtNode::new(ipv6_id, ipv6_addr),
    ];
    DhtPersistence::save_to_file_sync(&path, &self_id, &nodes)
        .expect("mixed-family fixture snapshot should be written");

    let engine4 = DhtEngine::start(DhtEngineConfig {
        dht_file_path: Some(path.clone()),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("IPv4 DHT engine should start");
    let engine6 = DhtEngine::start(DhtEngineConfig {
        listen_addr: Some("::1".parse().unwrap()),
        dht_file_path: Some(path),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("IPv6 DHT engine should start");

    let routing_table4 = engine4.context.task_context.routing_table.read().await;
    assert_eq!(routing_table4.total_node_count(), 1);
    assert_eq!(
        routing_table4.find_closest(&ipv4_id, 1)[0].addr(),
        ipv4_addr
    );
    drop(routing_table4);

    let routing_table6 = engine6.context.task_context.routing_table.read().await;
    assert_eq!(routing_table6.total_node_count(), 1);
    assert_eq!(
        routing_table6.find_closest(&ipv6_id, 1)[0].addr(),
        ipv6_addr
    );
    drop(routing_table6);

    tokio::join!(engine4.shutdown_async(), engine6.shutdown_async());
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
async fn configured_bootstrap_endpoint_retries_a_dropped_initial_ping() {
    const NODE_ID: [u8; 20] = [0xD4; 20];
    const INFO_HASH: [u8; 20] = [0xE5; 20];

    let responder = Arc::new(
        UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("local bootstrap responder should bind"),
    );
    let responder_addr = responder.local_addr().expect("responder address");
    let shutdown = CancellationToken::new();
    let responder_shutdown = shutdown.clone();
    let (retry_tx, retry_rx) = tokio::sync::oneshot::channel();
    let responder_task = tokio::spawn(async move {
        let mut retry_tx = Some(retry_tx);
        let mut ping_attempts = 0usize;
        let mut buf = [0u8; 4096];

        loop {
            let received = tokio::select! {
                _ = responder_shutdown.cancelled() => break,
                received = responder.recv_from(&mut buf) => received,
            };
            let (len, from) = received.expect("bootstrap datagram should be received");
            let query = DhtMessage::decode(&buf[..len]).expect("bootstrap query should decode");
            let method = query.q.as_ref().map(|method| method.0.as_str());
            let response = match method {
                Some("ping") => {
                    ping_attempts += 1;
                    if ping_attempts == 1 {
                        continue;
                    }
                    if let Some(retry_tx) = retry_tx.take() {
                        let _ = retry_tx.send(ping_attempts);
                    }
                    DhtMessageBuilder::ping_response(&query.t, &NODE_ID)
                }
                Some("find_node") if ping_attempts >= 2 => {
                    DhtMessageBuilder::find_node_response(&query.t, &NODE_ID, &[])
                }
                Some("get_peers") if ping_attempts >= 2 => {
                    DhtMessageBuilder::get_peers_response_with_peers(
                        &query.t,
                        &NODE_ID,
                        b"fixture-token",
                        &[],
                    )
                }
                _ => continue,
            };
            let encoded = response.encode();
            responder
                .send_to(&encoded, from)
                .await
                .expect("bootstrap response should be sent");
        }
    });

    let engine = DhtEngine::start(DhtEngineConfig {
        port: 0,
        listen_addr: Some("127.0.0.1".parse().expect("IPv4 loopback")),
        bootstrap_nodes: vec![responder_addr],
        query_timeout: Duration::from_millis(100),
        bootstrap_timeout: Duration::from_secs(2),
        ..DhtEngineConfig::default()
    })
    .await
    .expect("DHT engine should start with a local bootstrap node");

    engine
        .wait_until_ready(Duration::from_secs(1))
        .await
        .expect("bootstrap setup should become ready");
    let retry_attempts = tokio::time::timeout(Duration::from_secs(1), retry_rx)
        .await
        .ok()
        .and_then(Result::ok);
    let lookup_result = if retry_attempts.is_some() {
        Some(tokio::time::timeout(Duration::from_secs(1), engine.find_peers(&INFO_HASH)).await)
    } else {
        None
    };
    let stats = engine.stats().await;

    engine.shutdown_async().await;
    shutdown.cancel();
    responder_task
        .await
        .expect("local bootstrap responder should stop");

    assert!(
        retry_attempts.is_some(),
        "bootstrap must issue a retry after the first ping is dropped"
    );
    assert_eq!(
        retry_attempts.expect("retry attempt count should be sent"),
        2,
        "the fixture should answer exactly the second ping"
    );
    let lookup = lookup_result
        .expect("lookup should be started after the retry")
        .expect("lookup should finish after bootstrap retry")
        .expect("lookup should contact the responsive bootstrap node");
    assert!(lookup.nodes_contacted > 0);
    assert!(stats.good_nodes > 0, "the responsive node should be good");
    assert_eq!(
        stats.total_nodes, 1,
        "the verified response ID should replace its bootstrap placeholder"
    );
}
