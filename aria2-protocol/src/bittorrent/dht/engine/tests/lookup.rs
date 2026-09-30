use super::super::{DhtEngine, DhtEngineConfig};
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use crate::bittorrent::dht::node::DhtNode;
use crate::bittorrent::dht::task::DhtTask;
use crate::bittorrent::dht::task_impl::PingTask;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

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
            let encoded = response.encode();
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
        &engine.context.task_context.self_id,
        &engine.context.task_context.routing_table,
        &engine.context.task_context.socket,
        &engine.context.task_context.tracker,
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(lookup.peers, vec![peer]);
    assert_eq!(lookup.nodes_contacted, 1);
    assert_eq!(lookup.token_nodes.len(), 1);
    assert_eq!(lookup.token_nodes[0].0, responder_addr);
    assert_eq!(lookup.token_nodes[0].1, latest_node_id);
    assert_eq!(lookup.token_nodes[0].2, b"fixture-token");

    let routing_table = engine.context.task_context.routing_table.read().await;
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
async fn nodes_contacted_counts_sent_queries_even_when_nodes_do_not_reply() {
    const INFO_HASH: [u8; 20] = [0x42; 20];
    let silent_peer = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("silent DHT peer should bind");
    let silent_peer_addr = silent_peer.local_addr().expect("silent peer address");
    let engine = DhtEngine::start(DhtEngineConfig {
        query_timeout: Duration::from_millis(40),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should start");
    engine
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::unverified([0xE1; 20], silent_peer_addr));

    let lookup_engine = Arc::clone(&engine);
    let lookup = tokio::spawn(async move { lookup_engine.find_peers(&INFO_HASH).await });
    let mut packet = [0u8; 1024];
    let (packet_len, _) =
        tokio::time::timeout(Duration::from_secs(1), silent_peer.recv_from(&mut packet))
            .await
            .expect("lookup should send a query to the silent node")
            .expect("silent peer should receive the UDP query");
    let query = DhtMessage::decode(&packet[..packet_len]).expect("valid DHT query");
    assert!(query.is_query());

    let result = lookup
        .await
        .expect("lookup task should finish")
        .expect("lookup should return an empty result after timeout");
    assert!(result.peers.is_empty());
    assert_eq!(result.nodes_contacted, 1);

    engine.shutdown_async().await;
}

#[tokio::test]
async fn nodes_contacted_excludes_queries_rejected_by_the_udp_socket() {
    const INFO_HASH: [u8; 20] = [0x43; 20];
    let engine = DhtEngine::start(DhtEngineConfig {
        listen_addr: Some(std::net::Ipv4Addr::LOCALHOST.into()),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("DHT engine should start with an IPv4 socket");
    let ipv6_peer_addr: SocketAddr = "[::1]:6881".parse().expect("IPv6 peer address");
    engine
        .context
        .task_context
        .routing_table
        .write()
        .await
        .insert(DhtNode::unverified([0xE2; 20], ipv6_peer_addr));

    let result = engine
        .find_peers(&INFO_HASH)
        .await
        .expect("a local send failure should complete the lookup");
    assert_eq!(result.nodes_contacted, 0);

    engine.shutdown_async().await;
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
            let encoded = response.encode();
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

#[tokio::test]
async fn ping_timeouts_are_counted_once_and_promote_cached_nodes_at_threshold() {
    const QUERY_TIMEOUT: Duration = Duration::from_millis(20);

    let unreachable_peer = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("loopback DHT peer should bind");
    let unreachable_addr = unreachable_peer.local_addr().expect("peer address");
    let engine = DhtEngine::start(DhtEngineConfig {
        self_id: [0xFF; 20],
        query_timeout: QUERY_TIMEOUT,
        refresh_check_interval: Duration::from_secs(3_600),
        node_contact_interval: Duration::from_secs(3_600),
        cleanup_interval: Duration::from_secs(3_600),
        save_interval: Duration::from_secs(3_600),
        token_rotation_interval: Duration::from_secs(3_600),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("local DHT engine should start");

    let failed_node_id = [1; 20];
    {
        let mut table = engine.context.task_context.routing_table.write().await;
        for i in 1..=8u8 {
            let addr = if i == 1 {
                unreachable_addr
            } else {
                format!("127.0.0.1:{}", 7_000 + u16::from(i))
                    .parse()
                    .expect("loopback peer address")
            };
            table.insert(DhtNode::new([i; 20], addr));
        }
        table.insert(DhtNode::new([0x40; 20], "127.0.0.1:6990".parse().unwrap()));
        table.insert(DhtNode::new([0x41; 20], "127.0.0.1:6991".parse().unwrap()));
    }

    Box::new(PingTask::new(
        engine.context.task_context.clone(),
        DhtNode::new(failed_node_id, unreachable_addr),
        3,
        None,
    ))
    .run()
    .await;

    let stats = engine.stats().await;
    assert_eq!(stats.total_nodes, 8);
    assert_eq!(stats.bad_nodes, 0);
    assert_eq!(stats.cached_nodes, 2);
    {
        let table = engine.context.task_context.routing_table.read().await;
        let node = table
            .get_bucket_for(&failed_node_id)
            .nodes()
            .iter()
            .find(|node| node.id() == &failed_node_id)
            .expect("four consecutive timeouts must not evict the peer");
        assert_eq!(node.failed_count(), 4);
    }

    Box::new(PingTask::new(
        engine.context.task_context.clone(),
        DhtNode::new(failed_node_id, unreachable_addr),
        0,
        None,
    ))
    .run()
    .await;

    let stats = engine.stats().await;
    assert_eq!(stats.total_nodes, 8);
    assert_eq!(stats.bad_nodes, 0);
    assert_eq!(stats.cached_nodes, 1);
    engine.shutdown_async().await;
}

#[tokio::test]
async fn announce_peer_timeout_updates_routing_health_once() {
    const QUERY_TIMEOUT: Duration = Duration::from_millis(20);

    let unreachable_peer = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("local UDP peer should bind");
    let unreachable_addr = unreachable_peer.local_addr().expect("peer address");
    let engine = DhtEngine::start(DhtEngineConfig {
        query_timeout: QUERY_TIMEOUT,
        refresh_check_interval: Duration::from_secs(3_600),
        node_contact_interval: Duration::from_secs(3_600),
        cleanup_interval: Duration::from_secs(3_600),
        save_interval: Duration::from_secs(3_600),
        token_rotation_interval: Duration::from_secs(3_600),
        ..DhtEngineConfig::local()
    })
    .await
    .expect("local DHT engine should start");

    let node_id = [0x07; 20];
    {
        let mut table = engine.context.task_context.routing_table.write().await;
        table.insert(DhtNode::new(node_id, unreachable_addr));
        for _ in 0..3 {
            assert!(table.mark_bad(&node_id));
        }
    }

    let token_nodes = [(unreachable_addr, node_id, vec![1, 2, 3])];
    let sent_queries = crate::bittorrent::dht::lookup::announce_to_token_nodes(
        &[0x22; 20],
        &engine.context.task_context.self_id,
        6881,
        &token_nodes,
        &engine.context.task_context.socket,
        &engine.context.task_context.tracker,
        engine.context.task_context.query_timeout,
    )
    .await;
    assert_eq!(
        sent_queries, 1,
        "the public count records successful UDP sends"
    );

    crate::bittorrent::dht::lookup::announce_to_token_nodes_and_update_routing_table(
        &[0x22; 20],
        6881,
        &token_nodes,
        &engine.context.task_context,
    )
    .await;

    let table = engine.context.task_context.routing_table.read().await;
    let node = table
        .get_bucket_for(&node_id)
        .nodes()
        .iter()
        .find(|node| node.id() == &node_id)
        .expect("four failures must not evict the node");
    assert_eq!(node.failed_count(), 4);
    drop(table);
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
                let encoded = response.encode();
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
            "DHT UDP pressure: concurrency={max_concurrent_lookups}, requests={REQUESTS}, successful={successful}, responder_requests={received}, responder_responses={responses}, request_loss={:.2}%, response_loss={:.2}%, p50={:?}, p95={:?}, max={:?}, throughput={throughput:.1}/s",
            request_loss as f64 * 100.0 / expected_packets as f64,
            response_loss as f64 * 100.0 / expected_packets as f64,
            p50,
            p95,
            max,
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
