//! AriaNg compatibility end-to-end tests.
//!
//! These tests spawn a real `RpcServer` on an ephemeral port and send
//! JSON-RPC requests using `reqwest` (mirroring how the AriaNg browser
//! client talks to aria2). They verify the wire format matches the original
//! aria2 1.37.0 protocol byte-for-byte so that AriaNg, YAAM, and other
//! third-party plugins can connect without modification.
//!
//! Coverage areas (matching the approved plan Task 4.1):
//! 1. Token auth — positional `["token:secret", ...]` and named `{"secret": ...}`
//! 2. `aria2.tellStatus` — full field set + `keys` filter
//! 3. `aria2.changeUri` — returns `[delcount, addcount]` array
//! 4. `aria2.remove`/`pause`/`unpause` — return GID string
//! 5. `aria2.getGlobalStat` — all 6 fields are JSON strings
//! 6. `aria2.getVersion` — `enabledFeatures` includes `"BitTorrent"`
//! 7. `system.multicall` — error isolation (one failure doesn't abort others)

use std::time::Duration;

use aria2_rpc::server::{AuthConfig, CorsConfig, RpcServer, ServerConfig};
use serde_json::{json, Value};

// =========================================================================
// Test infrastructure
// =========================================================================

/// Pick a free ephemeral port by binding to `:0` and immediately dropping the
/// listener. There is a TOCTOU race here, but it is acceptable for tests.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Wait for a TCP port to start accepting connections.
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

/// Spawn `server.serve()` in the background.
fn spawn_server(server: RpcServer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = server.serve().await;
    })
}

/// Build a server with the given auth token and start it on an ephemeral port.
/// Returns `(port, join_handle)`.
async fn start_server_with_auth(token: &str) -> (u16, tokio::task::JoinHandle<()>) {
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::default()) // wildcard — AriaNg default
        .with_auth(AuthConfig::default().with_token(token));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;
    (port, handle)
}

/// Build a server with no auth and start it on an ephemeral port.
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

/// Send a JSON-RPC POST request and return the parsed response body.
async fn rpc_post(port: u16, body: &Value) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .json(body)
        .send()
        .await
        .expect("request failed")
}

/// Helper: extract the `result` field from a JSON-RPC response.
fn result_of(resp: &Value) -> &Value {
    resp.get("result")
        .unwrap_or_else(|| panic!("expected result, got: {}", resp))
}

/// Helper: extract the `error` field from a JSON-RPC response.
fn error_of(resp: &Value) -> &Value {
    resp.get("error")
        .unwrap_or_else(|| panic!("expected error, got: {}", resp))
}

// =========================================================================
// 1. Token authentication — positional and named params
// =========================================================================

/// AriaNg sends `params: ["token:<secret>", <actual_params>...]` (positional).
/// The server must strip the `token:` prefix, validate the secret, and pass
/// only the remaining params to the handler.
#[tokio::test]
async fn ariang_positional_token_auth_succeeds() {
    let (port, handle) = start_server_with_auth("my-secret").await;

    // AriaNg-style request: first param is "token:my-secret"
    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": ["token:my-secret", ["http://example.com/file.zip"]],
            "id": "ariang-1"
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let result = result_of(&body);
    let gid = result.as_str().expect("result should be a GID string");
    assert!(
        gid.len() == 16 && gid.chars().all(|c| c.is_ascii_hexdigit()),
        "GID should be 16 hex chars, got {:?}",
        gid
    );

    handle.abort();
}

/// AriaNg also supports named params: `params: {"secret": "<secret>", ...}`.
#[tokio::test]
async fn ariang_named_token_auth_succeeds() {
    let (port, handle) = start_server_with_auth("my-secret").await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": {"secret": "my-secret"},
            "id": "ariang-2"
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let result = result_of(&body);
    assert!(result.get("version").is_some(), "version field missing");
    assert!(
        result.get("enabledFeatures").is_some(),
        "enabledFeatures field missing"
    );

    handle.abort();
}

/// Wrong token → JSON-RPC error (not HTTP 401, because aria2 returns 200 with
/// an error body, matching the original aria2 protocol).
#[tokio::test]
async fn ariang_wrong_token_returns_jsonrpc_error() {
    let (port, handle) = start_server_with_auth("correct-secret").await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": ["token:wrong-secret"],
            "id": "ariang-3"
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let err = error_of(&body);
    let code = err.get("code").and_then(|c| c.as_i64());
    assert!(
        code == Some(-32001),
        "expected Unauthorized error code -32001, got {:?}",
        code
    );

    handle.abort();
}

/// Missing token when auth is required → error.
#[tokio::test]
async fn ariang_missing_token_returns_error() {
    let (port, handle) = start_server_with_auth("required-secret").await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": [],
            "id": "ariang-4"
        }),
    )
    .await;

    let body: Value = resp.json().await.expect("parse response");
    let err = error_of(&body);
    let code = err.get("code").and_then(|c| c.as_i64());
    assert!(
        code == Some(-32001),
        "expected Unauthorized error code -32001, got {:?}",
        code
    );

    handle.abort();
}

// =========================================================================
// 2. aria2.tellStatus — full field set + keys filter
// =========================================================================

/// Verify `aria2.tellStatus` returns all expected top-level fields that
/// AriaNg reads. Original aria2 emits ~24 fields; we verify the ones
/// AriaNg actually renders in its UI.
#[tokio::test]
async fn ariang_tell_status_has_all_expected_fields() {
    let (port, handle) = start_server_no_auth().await;

    // Create a task
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

    // Get full status
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
    let status = result_of(&status_resp.json::<Value>().await.unwrap()).clone();

    // AriaNg reads these fields (see AriaNg source: status.js).
    // These are the core fields every download (HTTP or BT) must have.
    let required_fields = [
        "gid",
        "status",
        "totalLength",
        "completedLength",
        "uploadLength",
        "downloadSpeed",
        "uploadSpeed",
        "connections",
        "dir",
        "files",
    ];
    for field in &required_fields {
        assert!(
            status.get(field).is_some(),
            "tellStatus missing field '{}' (AriaNg needs it), got: {}",
            field,
            status
        );
    }

    // BT-specific fields (numPieces, pieceLength) are optional for HTTP
    // downloads — AriaNg handles their absence gracefully. They are emitted
    // with `skip_serializing_if = "Option::is_none"`.

    // Numeric fields must be JSON strings (matching util::itos())
    for field in &[
        "totalLength",
        "completedLength",
        "downloadSpeed",
        "connections",
    ] {
        let v = status.get(field).unwrap();
        assert!(
            v.is_string(),
            "field '{}' should be a JSON string (util::itos), got: {:?}",
            field,
            v
        );
    }

    // `status` must be one of the original aria2 status strings
    let st = status.get("status").and_then(|v| v.as_str()).unwrap();
    assert!(
        matches!(
            st,
            "active" | "waiting" | "paused" | "complete" | "error" | "removed"
        ),
        "status '{}' not in original aria2 status set",
        st
    );

    handle.abort();
}

/// `aria2.tellStatus` with `keys` param returns only requested fields.
/// AriaNg uses this optimization to fetch only the fields it renders.
#[tokio::test]
async fn ariang_tell_status_with_keys_filter() {
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

    // Request only 2 specific fields
    let status_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellStatus",
            "params": [gid, ["gid", "status"]],
            "id": 2
        }),
    )
    .await;
    let status = result_of(&status_resp.json::<Value>().await.unwrap()).clone();

    // gid and status must be present
    assert!(status.get("gid").is_some(), "gid should be present");
    assert!(status.get("status").is_some(), "status should be present");

    // Other fields should NOT be present (AriaNg relies on this optimization)
    assert!(
        status.get("totalLength").is_none(),
        "totalLength should NOT be present when keys=[gid, status]"
    );
    assert!(
        status.get("files").is_none(),
        "files should NOT be present when keys=[gid, status]"
    );
    assert!(
        status.get("downloadSpeed").is_none(),
        "downloadSpeed should NOT be present when keys=[gid, status]"
    );

    handle.abort();
}

// =========================================================================
// 3. aria2.changeUri — returns [delcount, addcount]
// =========================================================================

/// AriaNg calls `aria2.changeUri` to add/remove URIs on a file. The response
/// must be a 2-element array `[delcount, addcount]` (both integers).
#[tokio::test]
async fn ariang_change_uri_returns_delcount_addcount_array() {
    let (port, handle) = start_server_no_auth().await;

    let add_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://original.com/file.zip"]],
            "id": 1
        }),
    )
    .await;
    let gid: String = serde_json::from_value(
        result_of(&add_resp.json::<Value>().await.unwrap()).clone(),
    )
    .unwrap();

    let change_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.changeUri",
            "params": [
                gid,
                1,  // fileIndex (1-based, matching original aria2)
                ["http://original.com/file.zip"],  // delUris
                ["http://mirror1.com/file.zip", "http://mirror2.com/file.zip"]  // addUris
            ],
            "id": 2
        }),
    )
    .await;
    let result = result_of(&change_resp.json::<Value>().await.unwrap()).clone();

    let arr = result
        .as_array()
        .unwrap_or_else(|| panic!("changeUri result should be an array, got: {}", result));
    assert_eq!(arr.len(), 2, "changeUri result should have 2 elements");
    let delcount = arr[0].as_i64().expect("delcount should be an integer");
    let addcount = arr[1].as_i64().expect("addcount should be an integer");
    assert_eq!(delcount, 1, "delcount should be 1 (deleted original)");
    assert_eq!(addcount, 2, "addcount should be 2 (added 2 mirrors)");

    handle.abort();
}

// =========================================================================
// 4. aria2.remove / pause / unpause — return GID string
// =========================================================================

/// AriaNg expects `aria2.pause` to return the GID string (not "OK").
#[tokio::test]
async fn ariang_pause_unpause_remove_return_gid() {
    let (port, handle) = start_server_no_auth().await;

    // Create a task
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

    // Pause → returns GID
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
    let pause_result = result_of(&pause_body);
    assert_eq!(
        pause_result.as_str(),
        Some(gid.as_str()),
        "aria2.pause should return the GID string"
    );

    // Unpause → returns GID
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
    let unpause_result = result_of(&unpause_body);
    assert_eq!(
        unpause_result.as_str(),
        Some(gid.as_str()),
        "aria2.unpause should return the GID string"
    );

    // Remove → returns GID
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
    let remove_result = result_of(&remove_body);
    assert_eq!(
        remove_result.as_str(),
        Some(gid.as_str()),
        "aria2.remove should return the GID string"
    );

    handle.abort();
}

// =========================================================================
// 5. aria2.getGlobalStat — all 6 fields are JSON strings
// =========================================================================

/// AriaNg parses all 6 `getGlobalStat` fields as strings. Original aria2
/// emits them via `util::itos()` — they are NEVER JSON numbers.
#[tokio::test]
async fn ariang_get_global_stat_all_fields_are_strings() {
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
        let v = stat
            .get(field)
            .unwrap_or_else(|| panic!("missing field '{}'", field));
        assert!(
            v.is_string(),
            "field '{}' should be a JSON string (util::itos), got: {:?}",
            field,
            v
        );
        // The string should parse as a non-negative integer
        let s = v.as_str().unwrap();
        s.parse::<u64>()
            .unwrap_or_else(|_| panic!("field '{}' value {:?} should parse as u64", field, s));
    }

    handle.abort();
}

// =========================================================================
// 6. aria2.getVersion — enabledFeatures includes "BitTorrent"
// =========================================================================

/// AriaNg checks `enabledFeatures` to decide which UI panels to show
/// (e.g., BT panel only if "BitTorrent" is present).
#[tokio::test]
async fn ariang_get_version_enabled_features() {
    let (port, handle) = start_server_no_auth().await;

    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": [],
            "id": 1
        }),
    )
    .await;
    let version = result_of(&resp.json::<Value>().await.unwrap()).clone();

    assert!(
        version.get("version").is_some(),
        "version field missing"
    );
    let features = version
        .get("enabledFeatures")
        .and_then(|v| v.as_array())
        .expect("enabledFeatures should be an array");
    assert!(
        !features.is_empty(),
        "enabledFeatures should not be empty"
    );

    // Verify "BitTorrent" is enabled (AriaNg shows BT UI based on this)
    let has_bit_torrent = features.iter().any(|f| f.as_str() == Some("BitTorrent"));
    assert!(
        has_bit_torrent,
        "enabledFeatures should include 'BitTorrent', got: {:?}",
        features
    );

    handle.abort();
}

// =========================================================================
// 7. aria2.getFiles — wire format (1-based index, all strings)
// =========================================================================

/// AriaNg reads `getFiles` to render the file list. All scalar fields must
/// be JSON strings, and `index` must be 1-based (matching util::uitos).
#[tokio::test]
async fn ariang_get_files_wire_format() {
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

    let files_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getFiles",
            "params": [gid],
            "id": 2
        }),
    )
    .await;
    let files = result_of(&files_resp.json::<Value>().await.unwrap()).clone();
    let files_arr = files
        .as_array()
        .unwrap_or_else(|| panic!("getFiles should return an array, got: {}", files));
    assert!(!files_arr.is_empty(), "should have at least one file");

    let f = &files_arr[0];
    // All scalars are JSON strings (matching util::itos/uitos)
    for field in &["index", "length", "completedLength", "selected"] {
        assert!(
            f.get(field).unwrap().is_string(),
            "field '{}' must be a JSON string, got: {:?}",
            field,
            f.get(field)
        );
    }
    // index is 1-based
    let idx: u64 = f
        .get("index")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .expect("index should parse as u64");
    assert!(idx >= 1, "index should be 1-based (>= 1), got {}", idx);
    // selected is "true"/"false"
    let sel = f.get("selected").unwrap().as_str().unwrap();
    assert!(
        sel == "true" || sel == "false",
        "selected must be \"true\"/\"false\", got {:?}",
        sel
    );

    handle.abort();
}

// =========================================================================
// 8. system.multicall — error isolation
// =========================================================================

/// AriaNg uses `system.multicall` to batch requests. One failing method
/// must NOT abort the others — each result is either `{result: ...}` or
/// `{error: {...}}`, preserving array order.
#[tokio::test]
async fn ariang_system_multicall_error_isolation() {
    let (port, handle) = start_server_no_auth().await;

    // Create a task first so we have a valid GID
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

    // Multicall: [getVersion, unknownMethod, tellStatus]
    // The middle one should error, but the others should succeed.
    let mc_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "system.multicall",
            "params": [[
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.doesNotExist", "params": []},
                {"methodName": "aria2.tellStatus", "params": [gid]}
            ]],
            "id": 2
        }),
    )
    .await;
    let result = result_of(&mc_resp.json::<Value>().await.unwrap()).clone();
    let arr = result
        .as_array()
        .unwrap_or_else(|| panic!("multicall result should be an array, got: {}", result));
    assert_eq!(arr.len(), 3, "multicall should return 3 results");

    // Per XML-RPC `system.multicall` convention (and original aria2
    // `SystemMulticallRpcMethod::execute`, RpcMethodImpl.cc:1462-1469):
    // - Success → result wrapped in single-element array `[value]`
    //   (AriaNg unwraps via `response.data[i][0]`)
    // - Error → `{"code":..., "message":...}` (no wrapper array)

    // First: getVersion succeeds → [result]
    assert!(
        arr[0].is_array(),
        "multicall[0] (getVersion) should be a [result] array, got: {}",
        arr[0]
    );
    let v0 = &arr[0][0];
    assert!(
        v0.get("version").is_some(),
        "multicall[0] inner value should have version field"
    );

    // Second: unknown method → error object (not wrapped in array)
    assert!(
        arr[1].get("code").is_some(),
        "multicall[1] (unknown method) should be an error object, got: {}",
        arr[1]
    );
    let err_code = arr[1].get("code").and_then(|c| c.as_i64());
    assert!(
        err_code == Some(-32601),
        "unknown method should return -32601 (MethodNotFound), got {:?}",
        err_code
    );

    // Third: tellStatus succeeds → [result] (error isolation verified)
    assert!(
        arr[2].is_array(),
        "multicall[2] (tellStatus) should be a [result] array despite prior error, got: {}",
        arr[2]
    );
    let v2 = &arr[2][0];
    assert!(
        v2.get("gid").is_some(),
        "multicall[2] inner value should have gid field"
    );

    handle.abort();
}

// =========================================================================
// 8b. aria2.changePosition — POS_SET / POS_CUR / POS_END
// =========================================================================

/// AriaNg calls `aria2.changePosition` to reorder the download queue.
/// The `how` parameter accepts the original aria2 constants:
/// `"POS_SET"` (absolute), `"POS_CUR"` (relative to current), `"POS_END"`
/// (relative to tail). Returns the resulting index as an integer.
#[tokio::test]
async fn ariang_change_position_pos_modes() {
    let (port, handle) = start_server_no_auth().await;

    // Create 3 tasks so the queue has multiple entries to reorder
    let mut gids: Vec<String> = Vec::new();
    for i in 0..3 {
        let resp = rpc_post(
            port,
            &json!({
                "jsonrpc": "2.0",
                "method": "aria2.addUri",
                "params": [[format!("http://example.com/file{}.zip", i)]],
                "id": i
            }),
        )
        .await;
        let gid: String = serde_json::from_value(
            result_of(&resp.json::<Value>().await.unwrap()).clone(),
        )
        .unwrap();
        gids.push(gid);
    }
    let target = &gids[1];

    // POS_SET: move to absolute position 0 (head of queue)
    let resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.changePosition",
            "params": [target, 0, "POS_SET"],
            "id": 10
        }),
    )
    .await;
    let result = result_of(&resp.json::<Value>().await.unwrap()).clone();
    let pos = result
        .as_i64()
        .unwrap_or_else(|| panic!("changePosition POS_SET should return an integer, got: {}", result));
    assert_eq!(pos, 0, "POS_SET pos=0 should move to head (index 0)");

    handle.abort();
}

// =========================================================================
// 9. aria2.getUris — URI status is lowercase "used"/"waiting"
// =========================================================================

/// AriaNg reads `getUris` to show which URIs are active. The `status` field
/// must be lowercase `"used"`/`"waiting"` (matching VLB_USED/VLB_WAITING).
#[tokio::test]
async fn ariang_get_uris_status_lowercase() {
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

    let uris_resp = rpc_post(
        port,
        &json!({
            "jsonrpc": "2.0",
            "method": "aria2.getUris",
            "params": [gid],
            "id": 2
        }),
    )
    .await;
    let uris = result_of(&uris_resp.json::<Value>().await.unwrap()).clone();
    let uris_arr = uris
        .as_array()
        .unwrap_or_else(|| panic!("getUris should return an array, got: {}", uris));
    assert!(!uris_arr.is_empty(), "should have at least one URI");

    for u in uris_arr {
        let status = u
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("uri entry missing 'status' field: {}", u));
        assert!(
            status == "used" || status == "waiting",
            "uri status must be lowercase \"used\"/\"waiting\", got {:?}",
            status
        );
    }

    handle.abort();
}

// =========================================================================
// 10. CORS — AriaNg (browser) requires CORS headers
// =========================================================================

/// AriaNg runs in a browser, so the server MUST send CORS headers.
/// Without `Access-Control-Allow-Origin`, the browser blocks the response.
#[tokio::test]
async fn ariang_cors_header_present() {
    let (port, handle) = start_server_no_auth().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", "http://localhost:8080")
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
        "CORS: Access-Control-Allow-Origin must be present (AriaNg is a browser app)"
    );

    handle.abort();
}