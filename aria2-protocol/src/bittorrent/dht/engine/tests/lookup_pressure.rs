use super::super::{DhtEngine, DhtEngineConfig};
use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

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
