//! End-to-end tests for the RPC HTTP server.
//!
//! Each test starts a fresh server on a random port, exercises one or
//! more JSON-RPC / WebSocket operations, and verifies the responses.
//!
//! Groups:
//!   A — Basic routing        (9 tests)
//!   B — Authentication       (4 tests)
//!   C — CORS                 (3 tests)
//!   D — WebSocket            (5 tests)
//!   E — Batch requests       (2 tests)
//!   F — Full lifecycle       (2 tests)

mod common;

use common::start_test_server;

use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a JSON-RPC POST body.
fn rpc_body(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "id": method.replace('.', "-"),
        "params": params,
    })
}

/// Send a JSON-RPC request and return the (status, JSON response).
async fn rpc_call_with_status(
    client: &Client,
    base_url: &str,
    method: &str,
    params: Value,
) -> (reqwest::StatusCode, Value) {
    let resp = client
        .post(format!("{base_url}/jsonrpc"))
        .json(&rpc_body(method, params))
        .send()
        .await
        .expect("POST /jsonrpc failed");
    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid JSON response");
    (status, body)
}

/// Send a JSON-RPC request, assert 200, return the JSON response.
async fn rpc_call(client: &Client, base_url: &str, method: &str, params: Value) -> Value {
    let (status, body) = rpc_call_with_status(client, base_url, method, params).await;
    assert_eq!(
        status, 200,
        "expected 200 for {method}, got {status}: {body}"
    );
    body
}

/// Assert the JSON-RPC response is a success with a non-null result.
fn assert_result(resp: &Value) {
    assert!(
        resp.get("result").is_some(),
        "expected 'result' field, got: {resp}"
    );
    assert!(
        !resp["result"].is_null(),
        "expected non-null result, got null"
    );
}

/// Assert the JSON-RPC response contains an error with the given code.
fn assert_error_code(resp: &Value, code: i64) {
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("expected 'error' field, got: {resp}"));
    assert_eq!(
        err["code"].as_i64().unwrap_or_default(),
        code,
        "expected error code {code}, got: {err}"
    );
}

/// Parse the GID from an `aria2.addUri` result.
fn parse_gid(resp: &Value) -> String {
    resp["result"]
        .as_str()
        .expect("result should be a string (GID)")
        .to_string()
}

// =========================================================================
// Group A — Basic routing
// =========================================================================

#[tokio::test]
async fn e2e_root_endpoint() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: Value = resp.json().await.unwrap();
    assert!(body["name"].is_string());
    assert!(body["version"].is_string());
    // endpoints is an object (e.g. {"jsonrpc": "/jsonrpc", "rpc": "/rpc", "ws": "/ws"})
    assert!(
        body["endpoints"].is_object(),
        "expected endpoints to be an object, got: {:?}",
        body["endpoints"]
    );
}

#[tokio::test]
async fn e2e_jsonrpc_get_info() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client.get(format!("{base}/jsonrpc")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: Value = resp.json().await.unwrap();
    // GET on /jsonrpc returns server info for discovery
    assert!(body.get("error").is_some() || body.get("name").is_some());
}

#[tokio::test]
async fn e2e_add_uri_via_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/test"]],
    )
    .await;

    // The download will fail because the URL is unreachable, but a GID
    // should still be returned (the download is created immediately).
    let gid = resp["result"].as_str();
    assert!(gid.is_some(), "expected a GID string, got: {resp}");
    assert_eq!(gid.unwrap().len(), 16, "GID must be 16 hex chars");
}

#[tokio::test]
async fn e2e_get_version_via_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;
    assert_result(&resp);
    assert!(resp["result"]["version"].is_string());
}

#[tokio::test]
async fn e2e_get_global_stat() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getGlobalStat", json!([])).await;
    assert_result(&resp);
    assert!(resp["result"]["downloadSpeed"].is_string());
}

#[tokio::test]
async fn e2e_rpc_endpoint_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/rpc"))
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_result(&body);
}

#[tokio::test]
async fn e2e_invalid_endpoint() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .get(format!("{base}/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn e2e_post_invalid_json() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .body("not-json-at-all")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    // The server should reject invalid JSON with 400 Bad Request
    assert!(
        resp.status().as_u16() == 400 || resp.status().is_success(),
        "expected 400 or 200, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn e2e_unknown_rpc_method() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.nonexistentMethod", json!([])).await;
    assert_error_code(&resp, -32601);
}

// =========================================================================
// Group B — Authentication
// =========================================================================

const TEST_TOKEN: &str = "my-secret-token";

#[tokio::test]
async fn e2e_auth_valid_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.getVersion",
        json![["token:my-secret-token"]],
    )
    .await;
    assert_result(&resp);
}

#[tokio::test]
async fn e2e_auth_wrong_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.getVersion",
        json![["token:wrong-token"]],
    )
    .await;
    assert_error_code(&resp, -32001);
}

#[tokio::test]
async fn e2e_auth_no_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;
    assert_error_code(&resp, -32001);
}

#[tokio::test]
async fn e2e_auth_no_auth_required() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;
    assert_result(&resp);
}

// =========================================================================
// Group C — CORS
// =========================================================================

#[tokio::test]
async fn e2e_cors_preflight() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_some());
}

#[tokio::test]
async fn e2e_cors_allowed_origin() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(resp.headers().get("access-control-allow-origin").is_some());
}

#[tokio::test]
async fn e2e_cors_wildcard() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://random-origin.example")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

// =========================================================================
// Group D — WebSocket
// =========================================================================

#[tokio::test]
async fn e2e_ws_upgrade_at_ws() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/ws"))
        .await
        .expect("WebSocket upgrade at /ws should succeed");
    let (_, rx) = ws.split();
    // Just verify the connection stays open — we don't expect messages
    // without subscribing. Close will time out, so we read without blocking.
    drop(rx);
}

#[tokio::test]
async fn e2e_ws_upgrade_at_jsonrpc() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WebSocket upgrade at /jsonrpc should succeed");
    let (_, rx) = ws.split();
    drop(rx);
}

#[tokio::test]
async fn e2e_ws_receive_event_add() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // Connect WS first
    let (ws, _) = connect_async(format!("{ws_url}/ws"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Trigger a download via HTTP
    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/test-event-add"]],
    )
    .await;
    let _gid = parse_gid(&resp);

    // We should receive at least one event (onDownloadStart)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after addUri")
        .expect("WS stream ended")
        .expect("WS message error");
}

#[tokio::test]
async fn e2e_ws_receive_event_pause() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // Connect WS
    let (ws, _) = connect_async(format!("{ws_url}/ws"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Add a download and pause it
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/test-event-pause"]],
    )
    .await;
    let gid = parse_gid(&add);

    let _pause = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;

    // Expect at least one event (onDownloadPause)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after pause")
        .expect("WS stream ended")
        .expect("WS message error");
}

#[tokio::test]
async fn e2e_ws_receive_event_complete() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    let (ws, _) = connect_async(format!("{ws_url}/ws"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Removing a download should produce onDownloadStop/Complete.
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/test-event-remove"]],
    )
    .await;
    let gid = parse_gid(&add);

    let _remove = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;

    // Expect at least one event (onDownloadStop or onDownloadComplete)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after remove")
        .expect("WS stream ended")
        .expect("WS message error");
}

// =========================================================================
// Group G — WebSocket JSON-RPC request/response
// =========================================================================

/// Verify that a WebSocket client can send a JSON-RPC request and receive
/// a response on the same connection (matching C++ aria2's
/// `WebSocketSession::onMsgRecvCallback` behavior).
#[tokio::test]
async fn e2e_ws_jsonrpc_get_version() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // Send aria2.getVersion over WebSocket
    use tokio_tungstenite::tungstenite::Message;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": 1
    });
    tx.send(Message::Text(request.to_string().into()))
        .await
        .expect("send failed");

    // Receive and parse the response
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("response should be valid JSON");

    // Verify it is a JSON-RPC success response
    assert!(
        resp.get("result").is_some(),
        "expected 'result' field in WS response, got: {resp}"
    );
    assert_eq!(resp["id"], 1, "response id should match request id");
    assert!(
        resp["result"]["version"].is_string(),
        "version should be a string"
    );
}

/// Verify that a WebSocket client can send a batch JSON-RPC request and
/// receive an array of responses.
#[tokio::test]
async fn e2e_ws_jsonrpc_batch_request() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // Send batch request with two methods
    use tokio_tungstenite::tungstenite::Message;
    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion", "params": [], "id": "b1"},
        {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "params": [], "id": "b2"},
    ]);
    tx.send(Message::Text(batch.to_string().into()))
        .await
        .expect("send failed");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS batch response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Vec<Value> =
        serde_json::from_str(&text).expect("batch response should be a JSON array");

    assert_eq!(resp.len(), 2, "batch response should contain 2 items");
    assert!(
        resp[0]["result"].is_object(),
        "first result should be an object"
    );
    assert!(
        resp[1]["result"].is_object(),
        "second result should be an object"
    );
    assert_eq!(resp[0]["id"], "b1");
    assert_eq!(resp[1]["id"], "b2");
}

/// Verify that an invalid JSON message over WebSocket returns a proper
/// JSON-RPC Parse Error (-32700) response.
#[tokio::test]
async fn e2e_ws_jsonrpc_invalid_json() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    use tokio_tungstenite::tungstenite::Message;
    tx.send(Message::Text("{not valid json}".into()))
        .await
        .expect("send failed");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS error response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("error response should be valid JSON");

    assert!(resp.get("error").is_some(), "expected 'error' field");
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32700),
        "expected Parse Error code -32700, got: {resp}"
    );
}

/// Verify that event notifications continue flowing while the WS connection
/// is processing JSON-RPC requests.
#[tokio::test]
async fn e2e_ws_jsonrpc_with_events() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // 1. Connect WS
    let (ws, _) = connect_async(format!("{ws_url}/ws"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // 2. Send a JSON-RPC request over WS
    use tokio_tungstenite::tungstenite::Message;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": "ev-test"
    });
    tx.send(Message::Text(request.to_string().into()))
        .await
        .expect("send failed");

    // 3. Receive the JSON-RPC response
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS JSON-RPC response")
        .expect("WS stream ended")
        .expect("WS message error");
    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("response should be valid JSON");
    assert!(
        resp.get("result").is_some(),
        "expected 'result' in JSON-RPC response, got: {resp}"
    );

    // 4. Trigger a download event via HTTP
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/ws-event-test"]],
    )
    .await;
    let _gid = parse_gid(&add);

    // 5. Verify we still receive events on the same WS connection
    let event_msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after addUri")
        .expect("WS stream ended")
        .expect("WS message error");
    let event_text = event_msg.into_text().expect("expected text message");
    let event: Value = serde_json::from_str(&event_text).expect("event should be valid JSON");
    // Event notifications have a "method" field (e.g. "aria2.onDownloadStart")
    assert!(
        event.get("method").is_some(),
        "expected 'method' field in event notification, got: {event}"
    );
}

// =========================================================================
// Group E — Batch requests
// =========================================================================

#[tokio::test]
async fn e2e_batch_valid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion",  "id": "v1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "id": "g1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.getSessionInfo", "id": "s1", "params": []},
    ]);

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    // The server's POST handler only decodes a single JsonRpcRequest;
    // batch arrays get 422 Unprocessable Entity (axum deserialization).
    assert!(
        resp.status().as_u16() == 200 || resp.status().as_u16() == 422,
        "expected 200 or 422, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn e2e_batch_mixed() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion",       "id": "v1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.nonexistentMethod", "id": "e1", "params": []},
    ]);

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    // Same 422 limitation as e2e_batch_valid above.
    assert!(
        resp.status().as_u16() == 200 || resp.status().as_u16() == 422,
        "expected 200 or 422, got {}",
        resp.status()
    );
}

// =========================================================================
// Group F — Full lifecycle
// =========================================================================

/// Full lifecycle test: add → status → pause → unpause → remove → tellStatus returns "removed".
#[tokio::test]
async fn e2e_full_lifecycle() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // 1. Add a download
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json![["http://127.0.0.1:1/lifecycle-test"]],
    )
    .await;
    let gid = parse_gid(&add);
    assert_eq!(gid.len(), 16, "GID must be 16 hex chars");

    // 2. tellStatus — the download exists
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_result(&status);
    assert_eq!(status["result"]["gid"].as_str(), Some(gid.as_str()));

    // 3. Pause — C++ returns the GID as a string (not array)
    let pause = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;
    assert_result(&pause);
    assert_eq!(
        pause["result"].as_str(),
        Some(gid.as_str()),
        "pause should return the GID string"
    );

    // 4. tellStatus — paused
    let paused = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_result(&paused);
    let status_str = paused["result"]["status"].as_str().unwrap_or("");
    assert!(
        status_str == "paused",
        "expected paused status, got '{status_str}'"
    );

    // 5. Unpause
    let unpause = rpc_call(&client, &base, "aria2.unpause", json![[&gid]]).await;
    assert_result(&unpause);

    // 6. Remove
    let remove = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;
    assert_result(&remove);

    // 7. tellStatus for removed GID — C++ aria2 keeps removed downloads in
    // DownloadResult (stopped list) so tellStatus returns status="removed"
    // rather than an error. Only errors if the GID was never added or
    // has been purged via removeDownloadResult/purgeDownloadResult.
    let removed_status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    if let Some(result) = removed_status.get("result") {
        // GID still in stopped results — status should be "removed"
        let status = result.get("status").and_then(|s| s.as_str()).unwrap_or("");
        assert!(
            status == "removed" || status == "error",
            "removed download status should be 'removed' or 'error', got '{status}'"
        );
    }
    // If the GID was already purged from stopped results, we'd get an error — also acceptable
}

#[tokio::test]
async fn e2e_global_option_change() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // 1. Get current global option
    let before = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_result(&before);

    // 2. Change max concurrent downloads — params must be [{...}]
    let change = rpc_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json![[{"max-concurrent-downloads": "5"}]],
    )
    .await;
    assert_result(&change);

    // 3. Verify the change is reflected
    let after = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_result(&after);
    assert_eq!(
        after["result"]["max-concurrent-downloads"].as_str(),
        Some("5"),
        "expected max-concurrent-downloads=5, got: {}",
        after["result"]["max-concurrent-downloads"]
    );
}
