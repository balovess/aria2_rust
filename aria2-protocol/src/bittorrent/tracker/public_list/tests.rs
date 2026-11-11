use super::*;

#[tokio::test]
async fn source_request_timeout_covers_the_complete_response_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\nhttp://slow.test/announce\n",
                )
                .await
                .unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let catalog = PublicTrackerList::new();
    assert_eq!(
        catalog.source_request_timeout().await,
        DEFAULT_TRACKER_SOURCE_TIMEOUT
    );
    catalog
        .set_source_request_timeout(Duration::from_millis(40))
        .await;
    let error = catalog
        .fetch_and_update(&format!("http://{address}/trackers.txt"))
        .await
        .expect_err("an incomplete response body should time out");
    assert!(error.contains("timed out"), "unexpected error: {error}");
    server.await.unwrap();

    catalog.set_source_request_timeout(Duration::ZERO).await;
    assert_eq!(
        catalog.source_request_timeout().await,
        DEFAULT_TRACKER_SOURCE_TIMEOUT
    );
}

#[test]
fn tracker_failure_kind_names_are_stable() {
    assert_eq!(TrackerFailureKind::Network.as_str(), "network");
    assert_eq!(TrackerFailureKind::Timeout.as_str(), "timeout");
    assert_eq!(
        TrackerFailureKind::RemoteTemporary.as_str(),
        "remoteTemporary"
    );
    assert_eq!(
        TrackerFailureKind::TrackerRejected.as_str(),
        "trackerRejected"
    );
    assert_eq!(
        TrackerFailureKind::MalformedResponse.as_str(),
        "malformedResponse"
    );
}

#[test]
fn test_parse_embedded_list() {
    let entries = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST);
    assert!(
        entries.len() >= 50,
        "embedded list should have at least 50 trackers, got {}",
        entries.len()
    );
}

#[test]
fn test_parse_empty_input() {
    let entries = PublicTrackerList::parse("");
    assert!(entries.is_empty(), "empty input should produce empty list");
}

#[test]
fn test_parse_whitespace_only() {
    let entries = PublicTrackerList::parse("\n\n   \n\t\n");
    assert!(entries.is_empty());
}

#[test]
fn test_parse_mixed_protocols() {
    let text = "http://a.com:80/announce\nhttps://b.com:443/announce\nudp://c.com:6969/announce\nwss://d.com:443/announce";
    let entries = PublicTrackerList::parse(text);
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].protocol, TrackerProtocol::Http);
    assert_eq!(entries[1].protocol, TrackerProtocol::Https);
    assert_eq!(entries[2].protocol, TrackerProtocol::Udp);
    assert_eq!(entries[3].protocol, TrackerProtocol::Wss);
}

#[test]
fn test_parse_invalid_lines_skipped() {
    let text = "http://valid.com:80/announce\nnot a url\n\nhttp://another.com/announce";
    let entries = PublicTrackerList::parse(text);
    assert_eq!(entries.len(), 2);
}

#[test]
fn test_get_http_trackers_filters_correctly() {
    let ptl = PublicTrackerList::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let http = ptl.get_http_trackers().await;
        for url in &http {
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "{} should be http or https",
                url
            );
            assert!(!url.starts_with("udp://"), "{} should not be udp", url);
        }
        assert!(!http.is_empty(), "should have at least some HTTP trackers");
    });
}

#[test]
fn test_get_udp_trackers_filters_correctly() {
    let ptl = PublicTrackerList::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let udp = ptl.get_udp_trackers().await;
        for url in &udp {
            assert!(url.starts_with("udp://"), "{} should be udp", url);
        }
        assert!(!udp.is_empty(), "should have at least some UDP trackers");
    });
}

#[tokio::test]
async fn test_stats_returns_reasonable_values() {
    let ptl = PublicTrackerList::new();
    let stats = ptl.stats().await;
    assert!(stats.total_entries >= 50);
    assert!(stats.http_count > 0);
    assert!(stats.udp_count > 0);
    assert!(
        stats.is_embedded_fallback,
        "default instance uses embedded list"
    );
    assert!(stats.last_updated.is_some());
}

#[tokio::test]
async fn catalog_subscribers_observe_configuration_and_entry_changes() {
    let ptl = PublicTrackerList::new();
    let mut updates = ptl.subscribe_updates();
    let mut revision = *updates.borrow_and_update();

    ptl.set_config(TrackerCatalogConfig {
        enabled: false,
        sources: Vec::new(),
        update_interval: Duration::from_secs(60),
    })
    .await;
    tokio::time::timeout(Duration::from_secs(1), updates.changed())
        .await
        .expect("configuration changes should wake subscribers")
        .expect("catalog update sender should remain open");
    let next_revision = *updates.borrow_and_update();
    assert_eq!(next_revision, revision.wrapping_add(1));
    revision = next_revision;

    let tracker = PublicTrackerList::parse("http://new.example/announce")
        .into_iter()
        .collect();
    ptl.replace_snapshot(tracker)
        .await
        .expect("test catalog snapshot should be valid");
    tokio::time::timeout(Duration::from_secs(1), updates.changed())
        .await
        .expect("entry changes should wake subscribers")
        .expect("catalog update sender should remain open");
    assert_eq!(*updates.borrow_and_update(), revision.wrapping_add(1));
}

#[tokio::test]
async fn test_default_instance_uses_embedded() {
    let ptl = PublicTrackerList::new();
    let all = ptl.get_all().await;
    let embedded_count = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST).len();
    assert_eq!(
        all.len(),
        embedded_count,
        "default instance should use all embedded entries"
    );
}

#[tokio::test]
async fn disabled_catalog_has_no_available_trackers() {
    let ptl = PublicTrackerList::new();
    ptl.set_config(TrackerCatalogConfig {
        enabled: false,
        sources: Vec::new(),
        update_interval: Duration::from_secs(60),
    })
    .await;

    assert!(ptl.available_snapshot().await.is_empty());
    assert!(ptl.get_http_trackers().await.is_empty());
    assert!(ptl.get_udp_trackers().await.is_empty());
    assert!(
        !ptl.get_all().await.is_empty(),
        "disabling availability must not erase the catalog snapshot"
    );
}

#[tokio::test]
async fn empty_source_list_skips_refresh_and_preserves_current_catalog() {
    let ptl = PublicTrackerList::new();
    let initial_snapshot = ptl.snapshot().await;

    ptl.set_config(TrackerCatalogConfig {
        enabled: true,
        sources: Vec::new(),
        update_interval: Duration::from_secs(60),
    })
    .await;

    assert_eq!(ptl.refresh().await.unwrap(), 0);
    assert_eq!(ptl.snapshot().await.as_ref(), initial_snapshot.as_ref());
    assert_eq!(ptl.last_refresh_error().await, None);
}

#[tokio::test]
async fn reenabled_catalog_exposes_existing_snapshot() {
    let ptl = PublicTrackerList::new();
    ptl.set_config(TrackerCatalogConfig {
        enabled: false,
        sources: Vec::new(),
        update_interval: Duration::from_secs(60),
    })
    .await;
    ptl.set_config(TrackerCatalogConfig {
        enabled: true,
        sources: Vec::new(),
        update_interval: Duration::from_secs(60),
    })
    .await;

    assert!(!ptl.available_snapshot().await.is_empty());
}

#[tokio::test]
async fn tracker_backoff_preserves_catalog_and_success_recovers() {
    let ptl = PublicTrackerList::new();
    let entry = PublicTrackerList::parse("http://unhealthy.example/announce")
        .into_iter()
        .next()
        .expect("test tracker should parse");
    ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

    ptl.record_failure(&entry.url).await;
    ptl.record_failure(&entry.url).await;
    assert!(ptl.contains(&entry.url).await);
    ptl.record_failure(&entry.url).await;

    assert!(ptl.contains(&entry.url).await);
    assert!(ptl.available_snapshot().await.is_empty());

    ptl.record_success(&entry.url).await;
    assert!(!ptl.available_snapshot().await.is_empty());
}

#[tokio::test]
async fn local_failures_do_not_cool_or_remove_tracker() {
    let ptl = PublicTrackerList::new();
    let entry = PublicTrackerList::parse("http://local-network.example/announce")
        .into_iter()
        .next()
        .expect("test tracker should parse");
    ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

    ptl.record_failure_kind(&entry.url, TrackerFailureKind::Network)
        .await;
    ptl.record_failure_kind(&entry.url, TrackerFailureKind::Timeout)
        .await;

    assert!(ptl.contains(&entry.url).await);
    assert!(
        ptl.available_snapshot()
            .await
            .iter()
            .any(|candidate| candidate.url == entry.url)
    );
}

#[tokio::test]
async fn remote_temporary_failure_is_retriable_and_preserves_catalog() {
    let ptl = PublicTrackerList::new();
    let entry = PublicTrackerList::parse("http://temporary.example/announce")
        .into_iter()
        .next()
        .expect("test tracker should parse");
    ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

    ptl.record_failure_kind(&entry.url, TrackerFailureKind::RemoteTemporary)
        .await;

    assert!(ptl.contains(&entry.url).await);
    assert!(ptl.available_snapshot().await.is_empty());
    ptl.record_success(&entry.url).await;
    assert!(!ptl.available_snapshot().await.is_empty());
}

#[tokio::test]
async fn explicit_tracker_failure_does_not_hide_or_remove_tracker() {
    let ptl = PublicTrackerList::new();
    let entry = PublicTrackerList::parse("http://rejected.example/announce")
        .into_iter()
        .next()
        .expect("test tracker should parse");
    ptl.replace_snapshot(vec![entry.clone()]).await.unwrap();

    ptl.record_failure_kind(&entry.url, TrackerFailureKind::TrackerRejected)
        .await;

    assert!(ptl.contains(&entry.url).await);
    assert!(
        ptl.available_snapshot()
            .await
            .iter()
            .any(|candidate| candidate.url == entry.url)
    );
}

#[test]
fn test_parse_non_announce_path_rejected() {
    let text = "http://valid.com:80/scrape\nhttp://valid.com:80/announce";
    let entries = PublicTrackerList::parse(text);
    assert_eq!(entries.len(), 1, "non-announce paths should be rejected");
    assert_eq!(entries[0].url, "http://valid.com:80/announce");
}

#[tokio::test]
async fn test_shutdown_sets_flag() {
    let ptl = PublicTrackerList::new();
    assert!(ptl.running.load(Ordering::Relaxed));
    ptl.shutdown();
    assert!(!ptl.running.load(Ordering::Relaxed));
}

#[tokio::test]
async fn test_start_auto_update_starts_task() {
    use std::time::Duration as StdDuration;
    let ptl = Arc::new(PublicTrackerList::new());
    ptl.start_auto_update(
        "https://example.com/fake.txt".to_string(),
        StdDuration::from_secs(999999),
    );
    sleep(Duration::from_millis(50)).await;
    assert!(ptl.running.load(Ordering::Relaxed));
    ptl.shutdown();
}
