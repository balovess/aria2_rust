use super::*;
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use futures::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

#[path = "tests/http_dynamic.rs"]
mod http_dynamic;
#[path = "tests/udp.rs"]
mod udp;

#[test]
fn runtime_snapshot_publishes_live_tracker_state() {
    let first = "http://tracker.example.com/one".to_string();
    let second = "udp://tracker.example.com/two".to_string();
    let shared = Arc::new(std::sync::RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![first.clone(), second.clone()]], &None);

    announcer.set_runtime_snapshot(Arc::clone(&shared));
    announcer.last_attempt_tracker_url = Some(second.clone());
    announcer.publish_runtime_snapshot();

    {
        let snapshot = shared.read().expect("tracker runtime snapshot lock");
        assert_eq!(
            snapshot.tracker_tiers,
            vec![vec![first.clone(), second.clone()]]
        );
        assert_eq!(
            snapshot.current_url.as_deref(),
            Some("http://tracker.example.com/one")
        );
        assert_eq!(snapshot.last_attempt_url.as_deref(), Some(second.as_str()));
        assert!(snapshot.announce_ready);
        assert!(!snapshot.all_failed);
        assert_eq!(snapshot.last_failure_kind, None);
        assert_eq!(snapshot.trackers.len(), 2);
        assert!(!snapshot.trackers[0].last_attempt);
        assert!(snapshot.trackers[1].last_attempt);
        assert_eq!(snapshot.trackers[0].seeders, None);
        assert_eq!(snapshot.trackers[1].seeders, None);
    }

    announcer
        .tracker_states
        .entry(first.clone())
        .or_default()
        .seeders = Some(11);
    announcer.last_failure_kind = Some(TrackerFailureKind::Timeout);
    announcer
        .tracker_states
        .entry(second.clone())
        .or_default()
        .last_failure_kind = Some(TrackerFailureKind::Timeout);
    announcer.publish_runtime_snapshot();
    let snapshot = shared.read().expect("tracker runtime snapshot lock");
    assert_eq!(
        snapshot.last_failure_kind,
        Some(TrackerFailureKind::Timeout)
    );
    assert_eq!(snapshot.trackers[0].seeders, Some(11));
    assert!(!snapshot.trackers[0].all_failed);
    assert_eq!(snapshot.trackers[0].last_failure_kind, None);
    assert_eq!(snapshot.trackers[1].seeders, None);
    assert!(snapshot.trackers[1].all_failed);
    assert_eq!(
        snapshot.trackers[1].last_failure_kind,
        Some(TrackerFailureKind::Timeout)
    );
}

#[test]
fn independent_announcers_merge_live_in_flight_state_without_overwriting() {
    let primary_url = "http://tracker.example.com/primary".to_string();
    let public_url = "http://tracker.example.com/public".to_string();
    let shared = Arc::new(std::sync::RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut primary = TrackerAnnouncer::new(&[vec![primary_url.clone()]], &None);
    primary.set_runtime_snapshot(Arc::clone(&shared));
    primary.last_attempt_tracker_url = Some(primary_url.clone());
    primary.tracker_attempt_started(&primary_url);
    primary.publish_runtime_snapshot();

    let mut public = primary.fork_public_tracker(&public_url);
    public.last_attempt_tracker_url = Some(public_url.clone());
    public.tracker_attempt_started(&public_url);
    public.publish_runtime_snapshot();

    let snapshot = shared.read().expect("tracker runtime snapshot lock");
    assert_eq!(snapshot.trackers.len(), 2);
    assert_eq!(snapshot.in_flight, 2);
    assert!(
        snapshot
            .trackers
            .iter()
            .all(|tracker| tracker.status == "announcing")
    );
}

#[test]
fn tracker_timeout_options_are_stored_for_both_transports() {
    let mut announcer = TrackerAnnouncer::new(
        &[vec!["http://tracker.example.com/announce".to_string()]],
        &None,
    );
    announcer.set_timeouts(Duration::from_secs(7), Duration::from_secs(11));

    assert_eq!(announcer.tracker_timeout_secs, 7);
    assert_eq!(announcer.tracker_connect_timeout_secs, 11);
}

#[test]
fn stopped_timeout_has_a_clear_default_and_can_be_configured() {
    let mut announcer = TrackerAnnouncer::new(&[], &None);
    assert_eq!(
        announcer.stopped_timeout,
        Duration::from_secs(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS)
    );
    announcer.set_stopped_timeout(Duration::ZERO);
    assert_eq!(announcer.stopped_timeout, Duration::from_millis(1));
}

#[tokio::test]
async fn stopped_announce_respects_total_shutdown_budget() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local tracker test listener");
    let address = listener.local_addr().expect("local tracker address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept tracker request");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await;
    });

    let url = format!("http://{address}/announce");
    let mut announcer = TrackerAnnouncer::new(&[vec![url]], &None);
    announcer.set_timeouts(Duration::from_secs(10), Duration::from_secs(10));
    announcer.set_stopped_timeout(Duration::from_millis(100));
    announcer.announce.set_runtime_halted(true);
    announcer.announce.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        announcer.announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0),
    )
    .await;
    assert!(result.is_ok(), "stopped announce exceeded shutdown budget");
    assert!(!announcer.stopped_sent);

    server.await.expect("tracker test server should exit");
}

#[tokio::test]
async fn tracker_request_timeout_aborts_a_slow_http_announce() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local tracker test listener");
    let address = listener.local_addr().expect("local tracker address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept tracker request");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await;
    });

    let url = format!("http://{address}/announce");
    let mut announcer = TrackerAnnouncer::new(&[vec![url]], &None);
    announcer.set_timeouts(Duration::from_secs(1), Duration::from_secs(1));

    let result = tokio::time::timeout(
        Duration::from_secs(4),
        announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
    )
    .await
    .expect("tracker request should finish within the test deadline");
    assert!(result.is_none(), "slow tracker request should time out");

    server.await.expect("tracker test server should exit");
}

#[tokio::test]
async fn http_tracker_reuses_one_policy_bound_connection_for_stopped_announce() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tracker reuse fixture");
    let address = listener
        .local_addr()
        .expect("tracker reuse fixture address");
    let response_body = {
        let mut response = BTreeMap::new();
        response.insert(b"interval".to_vec(), BencodeValue::Int(1));
        response.insert(b"peers".to_vec(), BencodeValue::Bytes(Vec::new()));
        BencodeValue::Dict(response).encode()
    };
    let server = tokio::spawn(async move {
        let (mut socket, peer) = listener.accept().await.expect("tracker should accept once");
        for _ in 0..2 {
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                socket
                    .read_exact(&mut byte)
                    .await
                    .expect("read tracker request");
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                response_body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write tracker response headers");
            socket
                .write_all(&response_body)
                .await
                .expect("write tracker response body");
        }
        peer.ip()
    });

    let tracker_url = format!("http://{address}/announce");
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_outbound_network_policy(Arc::new(OutboundNetworkPolicy::single(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    )));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
    announcer
        .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await
        .expect("initial tracker announce should succeed");
    announcer
        .announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await;

    assert_eq!(
        server.await.expect("tracker reuse fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn public_trackers_are_selected_separately_from_torrent_failover_tiers() {
    let catalog = Arc::new(PublicTrackerList::new());
    let existing_torrent_tracker = catalog
        .snapshot()
        .await
        .first()
        .expect("embedded catalog should contain a tracker")
        .url
        .clone();
    let mut announcer = TrackerAnnouncer::new(&[vec![existing_torrent_tracker.clone()]], &None);
    announcer.set_public_tracker_catalog(catalog, HashSet::new());

    let selected = announcer.public_tracker_urls(3).await;
    assert_eq!(selected.len(), 3);
    assert!(!selected.contains(&existing_torrent_tracker));
    assert_eq!(
        announcer
            .announce
            .announce_list()
            .get_tracker_url(0, 0)
            .map(String::as_str),
        Some(existing_torrent_tracker.as_str())
    );
    assert_eq!(announcer.announce.announce_list().tier_count(), 1);

    let available = announcer
        .available_public_tracker_urls(&HashSet::from([selected[0].clone()]), 3)
        .await;
    assert_eq!(available.len(), 3);
    assert!(!available.contains(&selected[0]));

    let fork = announcer.fork_public_tracker(&selected[0]);
    assert_eq!(
        fork.runtime_snapshot().tracker_tiers,
        vec![vec![selected[0].clone()]]
    );
}

#[tokio::test]
async fn excluded_public_trackers_are_not_added_after_refresh() {
    let catalog = Arc::new(PublicTrackerList::new());
    let mut announcer = TrackerAnnouncer::new(&[], &None);
    announcer.set_public_tracker_catalog(catalog, HashSet::new());
    announcer.set_excluded_tracker_urls(vec!["*".to_string()]);

    assert!(announcer.public_tracker_urls(3).await.is_empty());
    assert!(
        announcer
            .available_public_tracker_urls(&HashSet::new(), 3)
            .await
            .is_empty()
    );
    assert_eq!(announcer.announce.announce_list().tier_count(), 0);
}

#[tokio::test]
async fn websocket_tracker_announces_started_and_stopped_events() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local WebSocket tracker test listener");
    let address = listener.local_addr().expect("local tracker address");
    let server = tokio::spawn(async move {
        for expected_event in ["started", "stopped"] {
            let (stream, _) = listener.accept().await.expect("accept tracker request");
            let mut websocket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("complete WebSocket tracker handshake");
            let Some(Ok(Message::Text(request))) = websocket.next().await else {
                panic!("tracker did not receive a WebSocket announce")
            };
            let request: serde_json::Value =
                serde_json::from_str(request.as_ref()).expect("valid announce JSON");
            assert_eq!(request["action"], "announce");
            assert_eq!(request["event"], expected_event);
            assert_eq!(request["port"], 51413);

            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "interval": 60,
                        "complete": 2,
                        "incomplete": 3,
                        "peers": [{"ip": "192.0.2.20", "port": 6881}]
                    })
                    .to_string(),
                ))
                .await
                .expect("send tracker response");
        }
    });

    let mut announcer = TrackerAnnouncer::new(&[vec![format!("ws://{address}/announce")]], &None);
    let options = DownloadOptions {
        bt_tracker_timeout: 2,
        bt_tracker_connect_timeout: 2,
        ..DownloadOptions::default()
    };
    announcer.set_websocket_options(&options);
    announcer.set_tcp_port(51413);

    let result = announcer
        .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await
        .expect("started WebSocket announce should succeed");
    assert_eq!(result.event, AnnounceEvent::Started);
    assert_eq!(result.peers, vec![("192.0.2.20".to_string(), 6881)]);
    assert_eq!(result.interval, Duration::from_secs(60));
    assert_eq!(result.seeders, Some(2));
    assert_eq!(result.leechers, Some(3));

    announcer
        .announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await;
    assert!(announcer.stopped_sent);

    server
        .await
        .expect("WebSocket tracker test server should exit");
}

#[test]
fn test_tracker_announcer_creation() {
    let announcer = TrackerAnnouncer::new(&[], &None);
    assert!(!announcer.is_announce_ready());
    assert!(announcer.is_all_announce_failed());
}

#[test]
fn test_tracker_announcer_with_announce_url() {
    let urls = vec![vec!["http://tracker.example.com:6969/announce".to_string()]];
    let announcer = TrackerAnnouncer::new(&urls, &None);
    assert!(announcer.is_announce_ready());
}

#[test]
fn test_tracker_announcer_udp_detection() {
    let urls = vec![vec!["udp://tracker.example.com:6969/announce".to_string()]];
    let _announcer = TrackerAnnouncer::new(&urls, &None);
    assert!(is_udp_tracker("udp://tracker.example.com:6969/announce"));
    assert!(!is_udp_tracker("http://tracker.example.com:6969/announce"));
}

#[test]
fn test_announce_result_fields() {
    let result = AnnounceResult {
        peers: vec![("10.0.0.1".to_string(), 6881)],
        interval: Duration::from_secs(300),
        seeders: Some(5),
        leechers: Some(10),
        event: AnnounceEvent::Started,
        tracker_url: "udp://tracker.example.com:6969/announce".to_string(),
    };
    assert_eq!(result.peers.len(), 1);
    assert_eq!(result.seeders, Some(5));
    assert_eq!(result.leechers, Some(10));
}
