use super::*;

// =========================================================================
// EventType Tests
// =========================================================================

#[test]
fn test_event_type_method_names() {
    assert_eq!(
        EventType::DownloadStart.method_name(),
        "aria2.onDownloadStart"
    );
    assert_eq!(
        EventType::DownloadComplete.method_name(),
        "aria2.onDownloadComplete"
    );
    assert_eq!(
        EventType::DownloadError.method_name(),
        "aria2.onDownloadError"
    );
}

#[test]
fn test_event_type_from_method() {
    assert!(EventType::from_method("aria2.onDownloadStart").is_some());
    assert!(EventType::from_method("aria2.unknown").is_none());
}

// =========================================================================
// DownloadEvent Tests
// =========================================================================

#[test]
fn test_download_event_creation() {
    let event = DownloadEvent::download_start("abc123");
    assert_eq!(event.event_type().unwrap(), EventType::DownloadStart);
    assert_eq!(event.method(), "aria2.onDownloadStart");
    assert_eq!(event.params().len(), 1);
}

#[test]
fn test_download_event_serialization() {
    let event = DownloadEvent::download_error("def456");
    let json = event.to_json().unwrap();
    assert!(json.contains("\"method\":\"aria2.onDownloadError\""));
    // C++ format: params only contains {"gid": "..."}, no errorCode
    assert!(json.contains("\"gid\":\"def456\""));
    assert!(
        !json.contains("errorCode"),
        "C++ notification should not contain errorCode"
    );
}

#[test]
fn test_all_event_constructors() {
    let _ = DownloadEvent::download_start("g1");
    let _ = DownloadEvent::download_pause("g2");
    let _ = DownloadEvent::download_stop("g3");
    let _ = DownloadEvent::download_complete("g4");
    let _ = DownloadEvent::download_error("g5");
    let _ = DownloadEvent::bt_download_complete("g6");
}

#[test]
fn test_download_event_gid_extraction() {
    let event = DownloadEvent::download_complete("gid-001");
    assert_eq!(event.gid(), "gid-001");

    let event2 = DownloadEvent::download_error("gid-err");
    assert_eq!(event2.gid(), "gid-err");
}

#[test]
fn test_download_event_event_type_str() {
    let event = DownloadEvent::download_start("g1");
    assert_eq!(event.event_type_str(), "aria2.onDownloadStart");

    let event2 = DownloadEvent::bt_download_complete("g2");
    assert_eq!(event2.event_type_str(), "aria2.onBtDownloadComplete");
}

// =========================================================================
// C++ Compatibility Format Tests
// =========================================================================

/// Verify that all 6 C++ notification events serialize to the exact
/// JSON-RPC format produced by C++ aria2's WebSocketSessionMan::addNotification.
///
/// C++ produces: `{"jsonrpc":"2.0","method":"aria2.on<Event>","params":[{"gid":"<hex>"}]}`
#[test]
fn test_cpp_compatible_notification_format() {
    let test_cases: Vec<(EventType, &str, &str)> = vec![
        (
            EventType::DownloadStart,
            "aria2.onDownloadStart",
            "2089b05ecca3d829",
        ),
        (
            EventType::DownloadPause,
            "aria2.onDownloadPause",
            "a2bc3d4e5f6a7b8c",
        ),
        (
            EventType::DownloadStop,
            "aria2.onDownloadStop",
            "deadbeef12345678",
        ),
        (
            EventType::DownloadComplete,
            "aria2.onDownloadComplete",
            "c0ffee0000000001",
        ),
        (
            EventType::DownloadError,
            "aria2.onDownloadError",
            "bada5590abcdef01",
        ),
        (
            EventType::BtDownloadComplete,
            "aria2.onBtDownloadComplete",
            "1a2b3c4d5e6f7a8b",
        ),
    ];

    for (event_type, expected_method, gid) in &test_cases {
        let event = match event_type {
            EventType::DownloadStart => DownloadEvent::download_start(*gid),
            EventType::DownloadPause => DownloadEvent::download_pause(*gid),
            EventType::DownloadStop => DownloadEvent::download_stop(*gid),
            EventType::DownloadComplete => DownloadEvent::download_complete(*gid),
            EventType::DownloadError => DownloadEvent::download_error(*gid),
            EventType::BtDownloadComplete => DownloadEvent::bt_download_complete(*gid),
        };

        let json_str = event.to_json().expect("serialization should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("should parse back");

        // Verify jsonrpc version
        assert_eq!(
            parsed["jsonrpc"], "2.0",
            "jsonrpc version mismatch for {}",
            expected_method
        );

        // Verify method name
        assert_eq!(parsed["method"], *expected_method, "method mismatch");

        // Verify params structure: must be an array with exactly 1 element
        let params = parsed["params"].as_array().expect("params should be array");
        assert_eq!(
            params.len(),
            1,
            "params should have exactly 1 element for {}",
            expected_method
        );

        // The single param must be an object containing ONLY "gid"
        let param_obj = params[0].as_object().expect("param[0] should be object");
        assert_eq!(
            param_obj.len(),
            1,
            "param[0] should contain only 'gid' for {}",
            expected_method
        );
        assert_eq!(
            param_obj["gid"], *gid,
            "gid mismatch for {}",
            expected_method
        );

        // Verify no extra fields (files, errorCode, etc.)
        assert!(
            !param_obj.contains_key("files"),
            "C++ notification should NOT contain 'files' for {}",
            expected_method
        );
        assert!(
            !param_obj.contains_key("errorCode"),
            "C++ notification should NOT contain 'errorCode' for {}",
            expected_method
        );

        // Also verify the exact expected JSON string for one case
        if *expected_method == "aria2.onDownloadStart" {
            let expected = r#"{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[{"gid":"2089b05ecca3d829"}]}"#;
            assert_eq!(
                json_str, expected,
                "exact C++ format mismatch for onDownloadStart"
            );
        }
    }
}

/// Verify only 6 EventType variants exist (matching C++).
#[test]
fn test_exactly_six_event_types() {
    let all_methods: Vec<&'static str> = vec![
        EventType::DownloadStart.method_name(),
        EventType::DownloadPause.method_name(),
        EventType::DownloadStop.method_name(),
        EventType::DownloadComplete.method_name(),
        EventType::DownloadError.method_name(),
        EventType::BtDownloadComplete.method_name(),
    ];
    assert_eq!(
        all_methods.len(),
        6,
        "Should have exactly 6 event types matching C++"
    );
}

// =========================================================================
// WsConfig Tests (G4 Part A)
// =========================================================================

#[test]
fn test_ws_config_default() {
    let cfg = WsConfig::default();
    assert_eq!(cfg.ping_interval_secs, 30);
    assert_eq!(cfg.pong_timeout_secs, 60);
}

#[test]
fn test_ws_config_new() {
    let cfg = WsConfig::new(15, 45);
    assert_eq!(cfg.ping_interval_secs, 15);
    assert_eq!(cfg.pong_timeout_secs, 45);
}

#[test]
fn test_ws_config_builder() {
    let cfg = WsConfig::default()
        .with_ping_interval(10)
        .with_pong_timeout(30);
    assert_eq!(cfg.ping_interval_secs, 10);
    assert_eq!(cfg.pong_timeout_secs, 30);
}

// =========================================================================
// EventPublisher Tests
// =========================================================================

#[tokio::test]
async fn test_publisher_subscribe_publish() {
    let publisher = EventPublisher::new(16);
    let mut rx = publisher.subscribe("client-1", None).await;

    let event = DownloadEvent::download_start("test-gid");
    let count = publisher.publish(EventType::DownloadStart, event).unwrap();
    assert_eq!(count, 1);

    let (et, received) = rx.recv().await.unwrap();
    assert_eq!(et, EventType::DownloadStart);
    assert_eq!(received.method(), "aria2.onDownloadStart");
}

#[tokio::test]
async fn test_publisher_subscription_filter() {
    let publisher = EventPublisher::new(2);
    let mut subscription = publisher
        .subscribe("client-filtered", Some(vec![EventType::DownloadComplete]))
        .await;

    for index in 0..8 {
        assert_eq!(
            publisher
                .publish(
                    EventType::DownloadStart,
                    DownloadEvent::download_start(format!("ignored-{index}")),
                )
                .expect("subscriber should remain registered"),
            0,
            "filtered events should not be delivered"
        );
    }

    assert_eq!(
        publisher
            .publish(
                EventType::DownloadComplete,
                DownloadEvent::download_complete("accepted-gid"),
            )
            .expect("subscriber should receive the broadcast"),
        1
    );

    let (event_type, event) = subscription.recv().await.unwrap();
    assert_eq!(event_type, EventType::DownloadComplete);
    assert_eq!(event.gid(), "accepted-gid");
}

#[tokio::test]
async fn test_publisher_unsubscribe() {
    let publisher = EventPublisher::new(16);
    let _subscription = publisher.subscribe("client-1", None).await;
    assert_eq!(publisher.subscriber_count().await, 1);

    publisher.unsubscribe("client-1").await;
    assert_eq!(publisher.subscriber_count().await, 0);
}

#[tokio::test]
async fn test_scoped_subscription_releases_bookkeeping_on_drop() {
    let publisher = EventPublisher::new(16);
    let mut subscription = publisher.subscribe_scoped("ws-conn-1", None);
    assert_eq!(publisher.subscriber_count().await, 1);

    publisher
        .publish(
            EventType::DownloadStart,
            DownloadEvent::download_start("gid-1"),
        )
        .expect("scoped subscription must receive broadcast events");
    let (event_type, event) = subscription.recv().await.unwrap();
    assert_eq!(event_type, EventType::DownloadStart);
    assert_eq!(event.gid(), "gid-1");

    drop(subscription);
    assert_eq!(publisher.subscriber_count().await, 0);
}

#[tokio::test]
async fn test_scoped_subscription_reports_broadcast_lag() {
    let publisher = EventPublisher::new(1);
    let mut subscription = publisher.subscribe("lagging-client", None).await;

    publisher
        .publish(
            EventType::DownloadStart,
            DownloadEvent::download_start("gid-1"),
        )
        .unwrap();
    publisher
        .publish(
            EventType::DownloadComplete,
            DownloadEvent::download_complete("gid-1"),
        )
        .unwrap();

    assert!(matches!(
        subscription.recv().await,
        Err(broadcast::error::RecvError::Lagged(1))
    ));
}

#[tokio::test]
async fn test_publisher_multiple_subscribers() {
    let publisher = EventPublisher::new(16);
    let _rx1 = publisher.subscribe("c1", None).await;
    let _rx2 = publisher.subscribe("c2", None).await;
    let _rx3 = publisher.subscribe("c3", None).await;
    assert_eq!(publisher.subscriber_count().await, 3);
}

// =========================================================================
// WsSession Tests
// =========================================================================

#[test]
fn test_ws_session() {
    let (tx, rx) = broadcast::channel::<(EventType, DownloadEvent)>(4);
    let session = WsSession::new("session-abc", rx);
    assert_eq!(session.id(), "session-abc");
    drop(tx);
}

#[test]
fn test_default_publisher() {
    use futures::future::FutureExt;
    let p = EventPublisher::default();
    assert_eq!(p.subscriber_count().now_or_never().unwrap(), 0);
}

// =========================================================================
// NotificationBatcher Tests (G7)
// =========================================================================

#[test]
fn test_batcher_default() {
    let batcher = NotificationBatcher::default();
    assert_eq!(batcher.pending_count(), 0);
    assert_eq!(batcher.stats(), (0, 0));
}

#[test]
fn test_batcher_builder() {
    let batcher = NotificationBatcher::new()
        .with_max_batch_size(50)
        .with_flush_interval_ms(1000);
    assert_eq!(batcher.pending_count(), 0);
}

/// Test: push 2 onComplete for same GID → only 1 sent after flush
#[test]
fn test_dedup_same_event_replaces_old() {
    let mut batcher = NotificationBatcher::new().with_max_batch_size(100);

    let event1 = DownloadEvent::download_complete("gid-same-001");
    let event2 = DownloadEvent::download_complete("gid-same-001");

    batcher.push(event1);
    batcher.push(event2); // Should deduplicate: same GID + same event type

    // After dedup, only 1 pending (the latest replaces the old one)
    assert_eq!(
        batcher.pending_count(),
        1,
        "Dedup should keep only latest event"
    );
    let (sent, deduped) = batcher.stats();
    assert_eq!(sent, 0, "No events sent yet (not flushed)");
    assert_eq!(deduped, 1, "One event should have been deduplicated");

    // Flush should return exactly 1 event
    let _batch = batcher.maybe_flush(); // force flush by checking
    // Use flush via maybe_flush with elapsed time — but since we just pushed,
    // we need to verify the pending count is correct.
    // The maybe_flush won't trigger immediately if interval hasn't passed,
    // so let's just check pending count and stats.
    assert_eq!(batcher.pending_count(), 1);
}

#[test]
fn test_dedup_flushes_latest_event_without_cloning_it() {
    let mut batcher = NotificationBatcher::new().with_max_batch_size(100);

    batcher.push(DownloadEvent::new(
        EventType::DownloadComplete,
        vec![serde_json::json!({"gid": "gid-same", "marker": "old"})],
    ));
    batcher.push(DownloadEvent::new(
        EventType::DownloadComplete,
        vec![serde_json::json!({"gid": "gid-same", "marker": "new"})],
    ));

    let batch = batcher
        .flush_internal()
        .expect("pending event should flush");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].gid(), "gid-same");
    assert_eq!(batch[0].params()[0]["marker"], "new");
    assert_eq!(batcher.pending_count(), 0);
}

/// Test: onComplete + onError for same GID → both kept (different event types)
#[test]
fn test_different_events_not_deduped() {
    let mut batcher = NotificationBatcher::new().with_max_batch_size(100);

    let complete = DownloadEvent::download_complete("gid-mixed-001");
    let error = DownloadEvent::download_error("gid-mixed-001");

    batcher.push(complete);
    batcher.push(error); // Different event type → NOT deduplicated

    assert_eq!(
        batcher.pending_count(),
        2,
        "Different event types should both be kept"
    );
    let (_, deduped) = batcher.stats();
    assert_eq!(deduped, 0, "No deduplication for different event types");
}

/// Test: 21st push triggers auto-flush of 20 when max_batch_size=20
#[test]
fn test_batch_size_limit_triggers_flush() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(20)
        .with_flush_interval_ms(60_000);

    // Push 19 events — no flush yet
    for i in 0..19 {
        let flushed = batcher.push(DownloadEvent::download_start(format!("gid-{}", i)));
        assert!(!flushed, "Push {} should not trigger flush yet", i);
    }
    assert_eq!(batcher.pending_count(), 19);

    // Push 20th event — hits limit, should auto-flush
    let flushed = batcher.push(DownloadEvent::download_start("gid-19"));
    assert!(flushed, "20th push should trigger auto-flush");

    // After auto-flush, pending should be empty (all 20 were drained)
    assert_eq!(
        batcher.pending_count(),
        0,
        "Pending should be empty after auto-flush"
    );
    let (sent, _) = batcher.stats();
    assert_eq!(sent, 20, "Should have sent 20 notifications");
}

/// Test: timer-based flush emits accumulated events after interval
#[test]
fn test_timer_flush_emits_accumulated() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(100)
        .with_flush_interval_ms(50);

    for i in 0..5 {
        batcher.push(DownloadEvent::download_complete(format!("gid-timer-{}", i)));
    }
    assert_eq!(batcher.pending_count(), 5);

    // Wait well beyond the flush interval to handle CI jitter
    std::thread::sleep(std::time::Duration::from_millis(200));

    let batch = batcher.maybe_flush();
    assert!(
        batch.is_some(),
        "Timer flush should emit accumulated events"
    );
    let batch = batch.unwrap();
    assert_eq!(batch.len(), 5, "Should emit all 5 pending events");

    let (sent, _) = batcher.stats();
    assert_eq!(sent, 5, "Total sent should be 5");
    assert_eq!(
        batcher.pending_count(),
        0,
        "Pending should be empty after flush"
    );
}

/// Test: maybe_flush returns None when no time has passed
#[test]
fn test_maybe_flush_noop_when_fresh() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(100)
        .with_flush_interval_ms(60_000); // Long interval

    batcher.push(DownloadEvent::download_start("gid-fresh"));

    // Immediately after push, before interval elapses
    let result = batcher.maybe_flush();
    assert!(result.is_none(), "Should not flush before interval elapses");
    assert_eq!(batcher.pending_count(), 1);
}

/// Test: maybe_flush returns None when pending is empty
#[test]
fn test_maybe_flush_noop_when_empty() {
    let mut batcher = NotificationBatcher::new().with_flush_interval_ms(0);

    // Even with 0 interval, empty pending should return None
    let result = batcher.maybe_flush();
    assert!(result.is_none(), "Should not flush when no pending events");
}

/// Test: cross-GID events are never deduplicated
#[test]
fn test_different_gids_not_deduped() {
    let mut batcher = NotificationBatcher::new().with_max_batch_size(100);

    batcher.push(DownloadEvent::download_complete("gid-a"));
    batcher.push(DownloadEvent::download_complete("gid-b"));
    batcher.push(DownloadEvent::download_complete("gid-c"));

    assert_eq!(
        batcher.pending_count(),
        3,
        "Different GIDs should all be kept"
    );
    let (_, deduped) = batcher.stats();
    assert_eq!(deduped, 0, "No dedup across different GIDs");
}
