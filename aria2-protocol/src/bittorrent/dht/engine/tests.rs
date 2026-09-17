use super::{DhtEngine, DhtEngineConfig, DhtEngineState};
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::node::DhtNode;
use std::net::IpAddr;
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

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if engine.stats().await.good_nodes > 0 {
                break engine.find_peers(&[0x3Cu8; 20]).await;
            }
            if engine.state().await == DhtEngineState::Running {
                break engine.find_peers(&[0x3Cu8; 20]).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("public DHT bootstrap or lookup timed out")
    .expect("public DHT lookup failed");
    assert!(result.nodes_contacted > 0);
    engine.shutdown_async().await;
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
