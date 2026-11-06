#![cfg(feature = "bittorrent")]

use aria2_core::engine::bittorrent::tracker::communication::{
    TrackerAnnouncer, TrackerFailureKind, TrackerRuntimeSnapshot,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[tokio::test]
async fn silent_bep15_connect_is_reported_as_a_per_tracker_timeout() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind silent UDP tracker");
    let address = socket.local_addr().expect("UDP tracker address");
    let tracker_url = format!("udp://{address}/announce");
    let server = tokio::spawn(async move {
        let mut request = [0u8; 1500];
        let (length, _) = socket
            .recv_from(&mut request)
            .await
            .expect("receive BEP 15 connect request");
        assert_eq!(length, 16);
        assert_eq!(
            i32::from_be_bytes(request[8..12].try_into().expect("connect action")),
            0
        );
        std::future::pending::<()>().await;
    });

    let runtime = Arc::new(RwLock::new(TrackerRuntimeSnapshot::default()));
    let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url.clone()]], &None);
    announcer.set_runtime_snapshot(Arc::clone(&runtime));
    announcer.set_timeouts(Duration::from_secs(1), Duration::from_secs(1));

    assert!(
        tokio::time::timeout(
            Duration::from_secs(8),
            announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
        )
        .await
        .expect("UDP announce should finish its bounded retries")
        .is_none()
    );
    server.abort();
    let _ = server.await;

    let snapshot = runtime.read().expect("tracker runtime snapshot");
    assert_eq!(
        snapshot.last_failure_kind,
        Some(TrackerFailureKind::Timeout)
    );
    assert_eq!(
        snapshot
            .trackers
            .iter()
            .find(|tracker| tracker.uri == tracker_url)
            .and_then(|tracker| tracker.last_failure_kind),
        Some(TrackerFailureKind::Timeout)
    );
}
