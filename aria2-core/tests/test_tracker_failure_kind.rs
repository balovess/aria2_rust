#![cfg(feature = "bittorrent")]

use aria2_core::engine::bittorrent::tracker::communication::{
    TrackerAnnouncer, TrackerFailureKind, TrackerRuntimeSnapshot,
};
use aria2_core::request::request_group::DownloadOptions;
use futures::{SinkExt, StreamExt};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

async fn serve_http_response(
    status: &'static str,
    body: &'static [u8],
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local HTTP tracker");
    let address = listener.local_addr().expect("HTTP tracker address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept tracker connection");
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read tracker request line");
        assert!(line.starts_with("GET /announce?"));
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .await
                .expect("read tracker request header")
                == 0
                || line == "\r\n"
            {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        reader
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .expect("write tracker response headers");
        reader
            .get_mut()
            .write_all(body)
            .await
            .expect("write tracker response body");
    });
    (format!("http://{address}/announce"), server)
}

async fn http_failure_from_response(
    status: &'static str,
    body: &'static [u8],
) -> TrackerFailureKind {
    let (tracker_url, server) = serve_http_response(status, body).await;
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server.await.expect("HTTP tracker task should exit");
    runtime
        .read()
        .expect("tracker runtime snapshot")
        .last_failure_kind
        .expect("HTTP failure category should be published")
}

async fn serve_udp_tracker_error() -> (String, tokio::task::JoinHandle<()>) {
    serve_udp_tracker_response(3, b"unknown torrent").await
}

async fn serve_udp_tracker_response(
    action: i32,
    payload: &'static [u8],
) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind local UDP tracker");
    let address = socket.local_addr().expect("UDP tracker address");
    let server = tokio::spawn(async move {
        let mut request = [0u8; 1500];
        let (length, client) = socket
            .recv_from(&mut request)
            .await
            .expect("receive BEP 15 connect request");
        assert_eq!(length, 16);
        assert_eq!(
            i32::from_be_bytes(request[8..12].try_into().expect("connect action")),
            0
        );

        let mut response = Vec::with_capacity(16);
        response.extend_from_slice(&0i32.to_be_bytes());
        response.extend_from_slice(&request[12..16]);
        response.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
        socket
            .send_to(&response, client)
            .await
            .expect("send BEP 15 connect response");

        let (length, client) = socket
            .recv_from(&mut request)
            .await
            .expect("receive BEP 15 announce request");
        assert_eq!(length, 98);
        assert_eq!(
            i32::from_be_bytes(request[8..12].try_into().expect("announce action")),
            1
        );

        let mut response = Vec::with_capacity(8 + payload.len());
        response.extend_from_slice(&action.to_be_bytes());
        response.extend_from_slice(&request[12..16]);
        response.extend_from_slice(payload);
        socket
            .send_to(&response, client)
            .await
            .expect("send BEP 15 tracker response");
    });
    (format!("udp://{address}/announce"), server)
}

async fn serve_http_then_close() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind closing HTTP tracker");
    let address = listener.local_addr().expect("HTTP tracker address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept tracker connection");
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read tracker request line");
        assert!(line.starts_with("GET /announce?"));
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .await
                .expect("read tracker request header")
                == 0
                || line == "\r\n"
            {
                break;
            }
        }
    });
    (format!("http://{address}/announce"), server)
}

async fn websocket_failure_from_response(response: &'static str) -> TrackerFailureKind {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local WebSocket tracker");
    let address = listener.local_addr().expect("WebSocket tracker address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept tracker connection");
        let mut websocket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete WebSocket handshake");
        assert!(matches!(websocket.next().await, Some(Ok(Message::Text(_)))));
        websocket
            .send(Message::Text(response.to_string()))
            .await
            .expect("send tracker response");
    });

    let tracker_url = format!("ws://{address}/announce");
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_websocket_options(&DownloadOptions {
        bt_tracker_timeout: 2,
        bt_tracker_connect_timeout: 2,
        ..DownloadOptions::default()
    });

    let result = announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0).await;
    assert!(
        result.is_none(),
        "the test tracker returns a failure response"
    );
    server.await.expect("WebSocket tracker task should exit");
    runtime
        .read()
        .expect("tracker runtime snapshot")
        .last_failure_kind
        .expect("tracker failure category should be published")
}

#[tokio::test]
async fn websocket_announce_timeout_reaches_live_tracker_snapshot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local WebSocket tracker");
    let address = listener.local_addr().expect("WebSocket tracker address");
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept tracker connection");
        let mut websocket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete WebSocket handshake");
        assert!(matches!(websocket.next().await, Some(Ok(Message::Text(_)))));
        request_seen_tx.send(()).expect("report received announce");
        std::future::pending::<()>().await;
    });

    let tracker_url = format!("ws://{address}/announce");
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url.clone()]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_websocket_options(&DownloadOptions {
        bt_tracker_timeout: 1,
        bt_tracker_connect_timeout: 1,
        ..DownloadOptions::default()
    });

    let result = tokio::time::timeout(
        Duration::from_secs(4),
        announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
    )
    .await
    .expect("tracker actor should honor its timeout");
    assert!(result.is_none(), "the tracker intentionally never responds");
    tokio::time::timeout(Duration::from_secs(1), request_seen_rx)
        .await
        .expect("tracker should receive the announce before timeout")
        .expect("tracker task should report the announce");

    server.abort();
    let _ = server.await;

    let snapshot = runtime.read().expect("tracker runtime snapshot");
    assert_eq!(
        snapshot.last_failure_kind,
        Some(TrackerFailureKind::Timeout)
    );
    assert_eq!(snapshot.trackers.len(), 1);
    assert_eq!(snapshot.trackers[0].uri, tracker_url);
    assert_eq!(
        snapshot.trackers[0].last_failure_kind,
        Some(TrackerFailureKind::Timeout)
    );
}

#[tokio::test]
async fn websocket_rejection_and_malformed_response_are_distinguished() {
    assert_eq!(
        websocket_failure_from_response(r#"{"failure reason":"unknown torrent"}"#).await,
        TrackerFailureKind::TrackerRejected
    );
    assert_eq!(
        websocket_failure_from_response("not-json").await,
        TrackerFailureKind::MalformedResponse
    );
}

#[tokio::test]
async fn websocket_http_server_error_is_remote_temporary() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local WebSocket tracker");
    let address = listener.local_addr().expect("WebSocket tracker address");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept tracker connection");
        let mut stream = tokio::io::BufReader::new(stream);
        let mut request_line = String::new();
        stream
            .read_line(&mut request_line)
            .await
            .expect("read WebSocket handshake request line");
        assert!(request_line.starts_with("GET /announce "));
        loop {
            let mut header = String::new();
            stream
                .read_line(&mut header)
                .await
                .expect("read WebSocket handshake header");
            if header == "\r\n" {
                break;
            }
        }
        stream
            .get_mut()
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("send temporary tracker response");
    });

    let tracker_url = format!("ws://{address}/announce");
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_websocket_options(&DownloadOptions {
        bt_tracker_timeout: 2,
        bt_tracker_connect_timeout: 2,
        ..DownloadOptions::default()
    });

    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server.await.expect("WebSocket tracker task should exit");
    assert_eq!(
        runtime
            .read()
            .expect("tracker runtime snapshot")
            .last_failure_kind,
        Some(TrackerFailureKind::RemoteTemporary)
    );
}

#[tokio::test]
async fn concurrent_tracker_failures_remain_attached_to_their_urls() {
    let rejecting_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind rejecting tracker");
    let rejecting_address = rejecting_listener
        .local_addr()
        .expect("rejecting tracker address");
    let rejecting_server = tokio::spawn(async move {
        let (stream, _) = rejecting_listener
            .accept()
            .await
            .expect("accept rejecting announce");
        let mut websocket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete rejecting tracker handshake");
        assert!(matches!(websocket.next().await, Some(Ok(Message::Text(_)))));
        websocket
            .send(Message::Text(
                r#"{"failure reason":"not authorized"}"#.to_string(),
            ))
            .await
            .expect("send tracker rejection");
    });

    let timing_out_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind slow tracker");
    let timing_out_address = timing_out_listener
        .local_addr()
        .expect("slow tracker address");
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let timing_out_server = tokio::spawn(async move {
        let (stream, _) = timing_out_listener
            .accept()
            .await
            .expect("accept slow announce");
        let mut websocket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete slow tracker handshake");
        assert!(matches!(websocket.next().await, Some(Ok(Message::Text(_)))));
        request_seen_tx.send(()).expect("report slow announce");
        std::future::pending::<()>().await;
    });

    let rejecting_url = format!("ws://{rejecting_address}/announce");
    let timing_out_url = format!("ws://{timing_out_address}/announce");
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut primary = TrackerAnnouncer::new(&[vec![rejecting_url.clone()]], &None);
    primary.set_runtime_snapshot(Arc::clone(&runtime));
    primary.set_websocket_options(&DownloadOptions {
        bt_tracker_timeout: 1,
        bt_tracker_connect_timeout: 1,
        ..DownloadOptions::default()
    });
    let mut public = primary.fork_public_tracker(&timing_out_url);

    let (rejected, timed_out) = tokio::join!(
        primary.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
        public.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
    );
    assert!(rejected.is_none());
    assert!(timed_out.is_none());
    tokio::time::timeout(Duration::from_secs(1), request_seen_rx)
        .await
        .expect("slow tracker should receive the announce")
        .expect("slow tracker task should report the announce");
    rejecting_server
        .await
        .expect("rejecting tracker task should exit");
    timing_out_server.abort();
    let _ = timing_out_server.await;

    let snapshot = runtime.read().expect("tracker runtime snapshot");
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .find(|tracker| tracker.uri == rejecting_url)
            .and_then(|tracker| tracker.last_failure_kind),
        Some(TrackerFailureKind::TrackerRejected)
    );
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .find(|tracker| tracker.uri == timing_out_url)
            .and_then(|tracker| tracker.last_failure_kind),
        Some(TrackerFailureKind::Timeout)
    );
}

#[tokio::test]
async fn http_server_error_is_remote_temporary_in_tracker_runtime_snapshot() {
    let (tracker_url, server) = serve_http_response("503 Service Unavailable", b"").await;
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url.clone()]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server.await.expect("HTTP tracker task should exit");

    let snapshot = runtime.read().expect("tracker runtime snapshot");
    assert_eq!(
        snapshot.last_failure_kind,
        Some(TrackerFailureKind::RemoteTemporary)
    );
    let tracker = snapshot
        .trackers
        .iter()
        .find(|tracker| tracker.uri == tracker_url)
        .expect("failed HTTP tracker should have a runtime entry");
    assert_eq!(
        tracker.last_failure_kind,
        Some(TrackerFailureKind::RemoteTemporary)
    );
}

#[tokio::test]
async fn http_tracker_rejection_and_malformed_body_are_distinguished() {
    assert_eq!(
        http_failure_from_response("200 OK", b"d14:failure reason7:unknowne").await,
        TrackerFailureKind::TrackerRejected
    );
    assert_eq!(
        http_failure_from_response("200 OK", b"not-bencode").await,
        TrackerFailureKind::MalformedResponse
    );
}

#[tokio::test]
async fn http_transport_failure_is_network() {
    let (tracker_url, server) = serve_http_then_close().await;
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server.await.expect("closing HTTP tracker task should exit");
    assert_eq!(
        runtime
            .read()
            .expect("tracker runtime snapshot")
            .last_failure_kind,
        Some(TrackerFailureKind::Network)
    );
}

#[tokio::test]
async fn udp_bep15_tracker_error_is_rejected_and_attributed_to_its_url() {
    let (tracker_url, server) = serve_udp_tracker_error().await;
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url.clone()]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server.await.expect("UDP tracker task should exit");

    let snapshot = runtime.read().expect("tracker runtime snapshot");
    assert_eq!(
        snapshot.last_failure_kind,
        Some(TrackerFailureKind::TrackerRejected)
    );
    let tracker = snapshot
        .trackers
        .iter()
        .find(|tracker| tracker.uri == tracker_url)
        .expect("failed UDP tracker should have a runtime entry");
    assert_eq!(
        tracker.last_failure_kind,
        Some(TrackerFailureKind::TrackerRejected)
    );
}

#[tokio::test]
async fn udp_bep15_malformed_announce_response_is_reported() {
    let (tracker_url, server) = serve_udp_tracker_response(1, b"").await;
    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

    assert!(
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .is_none()
    );
    server
        .await
        .expect("malformed-response UDP tracker task should exit");
    assert_eq!(
        runtime
            .read()
            .expect("tracker runtime snapshot")
            .last_failure_kind,
        Some(TrackerFailureKind::MalformedResponse)
    );
}
