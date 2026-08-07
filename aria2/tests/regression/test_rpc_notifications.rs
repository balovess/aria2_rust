//! RPC notification regression tests for aria2-rust.
//!
//! These tests verify that WebSocket event notifications fire at the correct
//! lifecycle points for all supported event types (matching C++ aria2):
//! - aria2.onDownloadStart, onDownloadPause, onDownloadStop
//! - aria2.onDownloadComplete, onDownloadError
//! - aria2.onBtDownloadComplete

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::websocket::EventType;

/// Helper to create a JSON-RPC request.
fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

/// Helper to assert response is successful.
fn assert_success(resp: &aria2_rpc::json_rpc::JsonRpcResponse) {
    assert!(
        resp.is_success(),
        "Expected success response, got error: {:?}",
        resp.error
    );
}

/// Test: aria2.addUri fires onDownloadStart notification.
#[tokio::test]
async fn notification_add_uri_fires_download_start() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-start", None).await;

    let req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    // Should receive a DownloadStart event
    let (event_type, _event) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive notification within timeout")
        .expect("Should receive valid event");

    assert_eq!(event_type, EventType::DownloadStart);
}

/// Test: aria2.pause fires onDownloadPause notification.
#[tokio::test]
async fn notification_pause_fires_download_pause() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-pause", None).await;

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Consume the DownloadStart event
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;

    // Pause the task
    let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
    let pause_resp = engine.handle_request(&pause_req).await;
    assert_success(&pause_resp);

    // Should receive a DownloadPause event
    let (event_type, _event) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive notification within timeout")
        .expect("Should receive valid event");

    assert_eq!(event_type, EventType::DownloadPause);
}

/// Test: aria2.remove fires onDownloadStop notification.
#[tokio::test]
async fn notification_remove_fires_download_stop() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-stop", None).await;

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Consume the DownloadStart event
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;

    // Remove the task
    let remove_req = make_request("aria2.remove", serde_json::json!([gid]));
    let remove_resp = engine.handle_request(&remove_req).await;
    assert_success(&remove_resp);

    // Should receive a DownloadStop event
    let (event_type, _event) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive notification within timeout")
        .expect("Should receive valid event");

    assert_eq!(event_type, EventType::DownloadStop);
}

/// Test: aria2.unpause fires onDownloadStart notification (matching C++ behavior).
/// C++ aria2 does not have a separate onDownloadResume event; it reuses onDownloadStart.
#[tokio::test]
async fn notification_unpause_fires_download_start() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-resume", None).await;

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Consume the DownloadStart event from addUri
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;

    // Pause then unpause
    let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
    engine.handle_request(&pause_req).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;

    let unpause_req = make_request("aria2.unpause", serde_json::json!([gid]));
    let unpause_resp = engine.handle_request(&unpause_req).await;
    assert_success(&unpause_resp);

    // C++ aria2 fires onDownloadStart when a download is unpaused
    let (event_type, _event) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive notification within timeout")
        .expect("Should receive valid event");

    assert_eq!(event_type, EventType::DownloadStart);
}

/// Test: system.listNotifications returns all 6 C++ event types.
#[tokio::test]
async fn notification_list_notifications_returns_all_events() {
    let engine = RpcEngine::new();

    let req = make_request("system.listNotifications", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    assert!(notifications.contains(&"aria2.onDownloadStart".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadPause".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadStop".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadComplete".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadError".to_string()));
    assert!(notifications.contains(&"aria2.onBtDownloadComplete".to_string()));
    assert_eq!(notifications.len(), 6);
}

/// Test: forceRemove fires onDownloadStop notification.
#[tokio::test]
async fn notification_force_remove_fires_download_stop() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-force-stop", None).await;

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Consume the DownloadStart event
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;

    let force_req = make_request("aria2.forceRemove", serde_json::json!([gid]));
    let force_resp = engine.handle_request(&force_req).await;
    assert_success(&force_resp);

    let (event_type, _event) = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("Should receive notification within timeout")
        .expect("Should receive valid event");

    assert_eq!(event_type, EventType::DownloadStop);
}

/// Test: pauseAll fires onDownloadPause for each active task.
#[tokio::test]
async fn notification_pause_all_fires_pause_for_each_task() {
    let engine = RpcEngine::new();
    let mut rx = engine.publisher().subscribe("test-pause-all", None).await;

    // Add multiple tasks
    for i in 0..3 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([format!("http://example.com/file{}", i)]),
        );
        engine.handle_request(&req).await;
    }

    // Consume all DownloadStart events
    for _ in 1..=3 {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await;
    }

    let req = make_request("aria2.pauseAll", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    // Should receive 3 DownloadPause events
    let mut pause_count = 0;
    for _ in 1..=3 {
        if let Ok(Ok((event_type, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await
            && event_type == EventType::DownloadPause
        {
            pause_count += 1;
        }
    }
    assert_eq!(pause_count, 3, "Should receive 3 DownloadPause events");
}
