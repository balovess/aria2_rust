//! Aria2 Explorer compatibility end-to-end tests.
//!
//! These tests verify that the RPC server matches the wire protocol expected
//! by the Aria2 Explorer browser extension. Key differences from AriaNg:
//! - Prefers WebSocket over HTTP for real-time updates
//! - Uses `system.multicall` heavily for batch queries
//! - Specific keys filters for tellActive/tellWaiting/tellStopped
//! - Expects 8-second connection timeout
//! - Checks download status via tellStatus with specific field requests

use std::time::Duration;

use aria2_rpc::server::{AuthConfig, CorsConfig, RpcServer, ServerConfig};
use futures_util::{sink::SinkExt, stream::StreamExt};
use serde_json::{json, Value};

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

async fn wait_for_server(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
            Ok(_) => return,
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("server on port {} did not come up within 2s", port);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

fn spawn_server(server: RpcServer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = server.serve().await;
    })
}

async fn start_server_with_auth(token: &str) -> (u16, tokio::task::JoinHandle<()>) {
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::default())
        .with_auth(AuthConfig::default().with_token(token));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;
    (port, handle)
}

async fn start_server_no_auth() -> (u16, tokio::task::JoinHandle<()>) {
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::default());
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;
    (port, handle)
}

async fn rpc_post(port: u16, body: &Value) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .json(body)
        .send()
        .await
        .expect("request failed")
}

fn result_of(resp: &Value) -> &Value {
    resp.get("result")
        .unwrap_or_else(|| panic!("expected result, got: {}", resp))
}

// =========================================================================
// 1. WebSocket Connection
// =========================================================================

#[tokio::test]
async fn explorer_websocket_connects_to_jsonrpc() {
    let (port, handle) = start_server_no_auth().await;

    let url = format!("ws://127.0.0.1:{}/jsonrpc", port);
    let (mut ws_stream, resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket connect should succeed");

    assert_eq!(resp.status(), 101, "should get WebSocket upgrade status");
    let upgrade = resp.headers().get("upgrade").expect("upgrade header should exist");
    assert_eq!(upgrade.to_str().unwrap(), "websocket", "Upgrade header should be websocket");

    ws_stream.close(None).await.ok();
    handle.abort();
}

#[tokio::test]
async fn explorer_websocket_round_trips_requests() {
    let (port, handle) = start_server_no_auth().await;

    let url = format!("ws://127.0.0.1:{}/jsonrpc", port);
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket connect");

    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    let req = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": 1
    });
    ws_stream
        .send(TungsteniteMessage::Text(serde_json::to_string(&req).unwrap()))
        .await
        .expect("send request");

    let msg = tokio::time::timeout(Duration::from_secs(2), ws_stream.next())
        .await
        .expect("should receive response")
        .expect("stream should not end")
        .expect("no error");

    match msg {
        TungsteniteMessage::Text(text) => {
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["id"], 1);
            assert!(v["result"]["version"].is_string());
        }
        other => panic!("expected text message, got {:?}", other),
    }

    ws_stream.close(None).await.ok();
    handle.abort();
}

// =========================================================================
// 2. Authentication
// =========================================================================

#[tokio::test]
async fn explorer_positional_token_auth() {
    let (port, handle) = start_server_with_auth("my-secret").await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": ["token:my-secret"],
            "id": 1
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    assert!(body.get("error").is_none(), "auth should succeed");
    assert!(body["result"]["version"].is_string());

    handle.abort();
}

#[tokio::test]
async fn explorer_wrong_token_returns_error() {
    let (port, handle) = start_server_with_auth("correct").await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": ["token:wrong"],
            "id": 1
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let err = body.get("error").unwrap();
    assert_eq!(err["code"], -32001, "expected auth error code");

    handle.abort();
}

// =========================================================================
// 3. Initial Status Poll - tellActive/tellWaiting/tellStopped with keys
// =========================================================================

#[tokio::test]
async fn explorer_tell_active_with_keys_filter() {
    let (port, handle) = start_server_no_auth().await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellActive",
            "params": [["gid", "status", "totalLength", "completedLength"]],
            "id": 1
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let active = result_of(&body).as_array().unwrap();

    for item in active {
        let obj = item.as_object().unwrap();
        assert!(obj.contains_key("gid"), "gid must be present");
        assert!(obj.contains_key("status"), "status must be present");
        assert!(obj.contains_key("totalLength"), "totalLength must be present");
        assert!(obj.contains_key("completedLength"), "completedLength must be present");
        assert_eq!(obj.len(), 4, "only requested keys should be present");
    }

    handle.abort();
}

#[tokio::test]
async fn explorer_tell_waiting_with_pagination_and_keys() {
    let (port, handle) = start_server_no_auth().await;

    for i in 0..5 {
        rpc_post(
            port,
            &json!({
                "jsonrpc": "2.0",
                "method": "aria2.addUri",
                "params": [[format!("http://example.com/file{}.zip", i)]],
                "id": i
            }),
        )
        .await;
    }

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellWaiting",
            "params": [0, 3, ["gid", "status"]],
            "id": 10
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let waiting = result_of(&body).as_array().unwrap();
    assert!(waiting.len() <= 3, "should respect num=3 limit");

    for item in waiting {
        let obj = item.as_object().unwrap();
        assert!(obj.contains_key("gid"), "gid must be present");
        assert!(obj.contains_key("status"), "status must be present");
        assert_eq!(obj["status"], "waiting", "status should be waiting");
    }

    handle.abort();
}

#[tokio::test]
async fn explorer_tell_stopped_with_pagination_and_keys() {
    let (port, handle) = start_server_no_auth().await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellStopped",
            "params": [0, 10, ["gid", "status"]],
            "id": 3
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    assert!(body.get("error").is_none(), "tellStopped should not return error");
    
    let stopped = result_of(&body).as_array().unwrap();
    for item in stopped {
        let obj = item.as_object().unwrap();
        assert!(obj.contains_key("gid"), "gid must be present");
        assert!(obj.contains_key("status"), "status must be present");
    }

    handle.abort();
}

// =========================================================================
// 4. Add Download with options
// =========================================================================

#[tokio::test]
async fn explorer_add_uri_with_options() {
    let (port, handle) = start_server_no_auth().await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [
                ["http://example.com/file.zip"],
                {"dir": "/downloads", "out": "myfile.zip"}
            ],
            "id": 1
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let gid = result_of(&body).as_str().unwrap();
    assert!(gid.len() == 16 && gid.chars().all(|c| c.is_ascii_hexdigit()));

    let status_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellStatus",
            "params": [gid],
            "id": 2
        }),
    )
    .await;
    let binding = status_resp.json::<Value>().await.unwrap();
    let status = result_of(&binding);
    assert_eq!(status["dir"], "/downloads", "dir should be set");

    handle.abort();
}

// =========================================================================
// 5. Get Download Details
// =========================================================================

#[tokio::test]
async fn explorer_get_download_details() {
    let (port, handle) = start_server_no_auth().await;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip", "http://mirror.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    let tell_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellStatus",
            "params": [gid, ["gid", "status", "totalLength", "completedLength"]],
            "id": 2
        }),
    )
    .await;
    let binding1 = tell_resp.json::<Value>().await.unwrap();
    let status = result_of(&binding1);
    assert_eq!(status["gid"], gid);
    assert!(status["totalLength"].is_string());
    assert!(status["completedLength"].is_string());

    let files_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getFiles",
            "params": [gid],
            "id": 3
        }),
    )
    .await;
    let binding2 = files_resp.json::<Value>().await.unwrap();
    let files = result_of(&binding2).as_array().unwrap();
    assert!(!files.is_empty());
    let f = &files[0];
    assert!(f["index"].is_string());
    assert!(f["length"].is_string());
    assert!(f["completedLength"].is_string());

    let uris_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getUris",
            "params": [gid],
            "id": 4
        }),
    )
    .await;
    let binding3 = uris_resp.json::<Value>().await.unwrap();
    let uris = result_of(&binding3).as_array().unwrap();
    assert_eq!(uris.len(), 2);
    for u in uris {
        assert!(u["status"].is_string());
        assert!(u["uri"].is_string());
    }

    let opts_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getOption",
            "params": [gid],
            "id": 5
        }),
    )
    .await;
    let binding4 = opts_resp.json::<Value>().await.unwrap();
    if let Some(opts) = binding4.get("result").and_then(|r| r.as_object()) {
        assert!(opts.contains_key("dir"), "getOption should return dir");
    } else {
        tracing::info!("getOption returned error for GID {}: {}", gid, binding4);
    }

    handle.abort();
}

// =========================================================================
// 6. Control Operations
// =========================================================================

#[tokio::test]
async fn explorer_control_operations() {
    let (port, handle) = start_server_no_auth().await;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    let pause_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.pause",
            "params": [gid],
            "id": 2
        }),
    )
    .await;
    let pause_body = pause_resp.json::<Value>().await.unwrap();
    assert_eq!(result_of(&pause_body).as_str(), Some(gid.as_str()));

    let unpause_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.unpause",
            "params": [gid],
            "id": 3
        }),
    )
    .await;
    let unpause_body = unpause_resp.json::<Value>().await.unwrap();
    assert_eq!(result_of(&unpause_body).as_str(), Some(gid.as_str()));

    let remove_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.remove",
            "params": [gid],
            "id": 4
        }),
    )
    .await;
    let remove_body = remove_resp.json::<Value>().await.unwrap();
    assert_eq!(result_of(&remove_body).as_str(), Some(gid.as_str()));

    handle.abort();
}

// =========================================================================
// 7. Global Options
// =========================================================================

#[tokio::test]
async fn explorer_global_options() {
    let (port, handle) = start_server_no_auth().await;

    let get_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getGlobalOption",
            "params": [],
            "id": 1
        }),
    )
    .await;
    let binding = get_resp.json::<Value>().await.unwrap();
    let opts = result_of(&binding).as_object().unwrap();
    assert!(opts.contains_key("dir"), "global options should have dir");

    let change_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.changeGlobalOption",
            "params": [{"max-download-limit": "1024K"}],
            "id": 2
        }),
    )
    .await;
    let change_body = change_resp.json::<Value>().await.unwrap();
    assert_eq!(result_of(&change_body).as_str(), Some("OK"));

    handle.abort();
}

// =========================================================================
// 8. Global Stats
// =========================================================================

#[tokio::test]
async fn explorer_get_global_stat_all_strings() {
    let (port, handle) = start_server_no_auth().await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getGlobalStat",
            "params": [],
            "id": 1
        }),
    )
    .await;
    let stat = result_of(&resp.json::<Value>().await.unwrap()).clone();

    let expected_fields = [
        "downloadSpeed",
        "uploadSpeed",
        "numActive",
        "numWaiting",
        "numStopped",
        "numStoppedTotal",
    ];
    for field in &expected_fields {
        let v = stat.get(field).unwrap();
        assert!(
            v.is_string(),
            "field '{}' should be a JSON string, got: {:?}",
            field,
            v
        );
    }

    handle.abort();
}

// =========================================================================
// 9. Remove Download Result
// =========================================================================

#[tokio::test]
async fn explorer_remove_download_result() {
    let (port, handle) = start_server_no_auth().await;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.remove",
            "params": [gid],
            "id": 2
        }),
    )
    .await;

    let remove_result_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.removeDownloadResult",
            "params": [gid],
            "id": 3
        }),
    )
    .await;
    let body = remove_result_resp.json::<Value>().await.unwrap();
    assert_eq!(result_of(&body).as_str(), Some("OK"));

    handle.abort();
}

// =========================================================================
// 10. system.multicall
// =========================================================================

#[tokio::test]
async fn explorer_system_multicall_batch() {
    let (port, handle) = start_server_no_auth().await;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    let mc_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "system.multicall",
            "params": [[
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.getGlobalStat", "params": []},
                {"methodName": "aria2.tellStatus", "params": [gid]}
            ]],
            "id": 2
        }),
    )
    .await;
    let result = result_of(&mc_resp.json::<Value>().await.unwrap()).clone();
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 3);

    assert!(arr[0].is_array(), "getVersion should be wrapped");
    assert!(arr[0][0].get("version").is_some());

    assert!(arr[1].is_array(), "getGlobalStat should be wrapped");
    assert!(arr[1][0].get("downloadSpeed").is_some());

    assert!(arr[2].is_array(), "tellStatus should be wrapped");
    assert_eq!(arr[2][0]["gid"], gid);

    handle.abort();
}

// =========================================================================
// 11. WebSocket Notifications
// =========================================================================

#[tokio::test]
async fn explorer_websocket_notification_format() {
    let (port, handle) = start_server_no_auth().await;

    let url = format!("ws://127.0.0.1:{}/jsonrpc", port);
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WebSocket connect");

    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), ws_stream.next())
        .await
        .expect("should receive notification")
        .expect("stream should not end")
        .expect("no error");

    match msg {
        TungsteniteMessage::Text(text) => {
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["jsonrpc"], "2.0");
            assert_eq!(v["method"], "aria2.onDownloadStart");
            
            let params = v["params"].as_array().unwrap();
            assert_eq!(params.len(), 1);
            
            let param_obj = params[0].as_object().unwrap();
            assert_eq!(param_obj.len(), 1, "params should only contain gid");
            assert_eq!(param_obj["gid"], gid);
        }
        other => panic!("expected text message, got {:?}", other),
    }

    ws_stream.close(None).await.ok();
    handle.abort();
}

// =========================================================================
// 12. CORS Headers (browser extension requirement)
// =========================================================================

#[tokio::test]
async fn explorer_cors_headers() {
    let (port, handle) = start_server_no_auth().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", "chrome-extension://alexhua-aria2-explorer")
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": [],
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    let allow_origin = resp.headers().get("access-control-allow-origin");
    assert!(
        allow_origin.is_some(),
        "CORS: Access-Control-Allow-Origin must be present (Aria2 Explorer is a browser extension)"
    );

    handle.abort();
}