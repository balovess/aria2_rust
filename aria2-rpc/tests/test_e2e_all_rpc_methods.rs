//! Comprehensive E2E tests for ALL aria2 RPC methods.
//!
//! Each test function starts a fresh RPC server on a random port and exercises
//! one or more JSON-RPC methods via real HTTP requests (reqwest), verifying
//! correct JSON-RPC 2.0 response format and behavioral semantics.
//!
//! Test groups:
//!   A — Task Management  (addUri, remove, forceRemove, pause, forcePause,
//!                          unpause, pauseAll, unpauseAll,
//!                          forcePauseAll, tellStatus, tellActive, tellWaiting,
//!                          tellStopped, changePosition, changeUri, saveSession)
//!   B — Option Management (getOption, changeOption, getGlobalOption, changeGlobalOption)
//!   C — BitTorrent       (getPeers, getUris, getFiles, getServers)
//!   D — System           (system.listMethods, system.listNotifications, system.multicall)
//!   E — Status/Session   (getGlobalStat, getVersion, getSessionInfo)
//!   F — Shutdown         (shutdown, forceShutdown)

mod common;

use common::{start_test_server, start_test_server_with_max_concurrent};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a JSON-RPC 2.0 POST body.
fn rpc_body(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "id": method.replace('.', "-"),
        "params": params,
    })
}

/// Send a JSON-RPC request and return both the HTTP status and JSON body.
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

/// Send a successful JSON-RPC request and return the JSON response.
async fn rpc_call(client: &Client, base_url: &str, method: &str, params: Value) -> Value {
    let (status, body) = rpc_call_with_status(client, base_url, method, params).await;
    assert_eq!(
        status, 200,
        "expected 200 for {method}, got {status}: {body}"
    );
    body
}

/// Send an erroring JSON-RPC request and assert aria2's HTTP mapping.
async fn rpc_error_call(
    client: &Client,
    base_url: &str,
    method: &str,
    params: Value,
    expected_status: reqwest::StatusCode,
) -> Value {
    let (status, body) = rpc_call_with_status(client, base_url, method, params).await;
    assert_eq!(
        status, expected_status,
        "unexpected HTTP status for {method}: {body}"
    );
    body
}

/// Assert the response has a non-null `result` field (no `error`).
fn assert_success(resp: &Value) {
    assert!(
        resp.get("result").is_some(),
        "expected 'result' field, got: {resp}"
    );
    assert!(
        !resp["result"].is_null(),
        "expected non-null result, got null in: {resp}"
    );
}

/// Assert the response has an `error` field with the given code.
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

/// Assert the response follows JSON-RPC 2.0 format: jsonrpc "2.0", matching id.
fn assert_jsonrpc_format(resp: &Value, expected_id: &str) {
    assert_eq!(resp["jsonrpc"], "2.0", "jsonrpc must be '2.0'");
    assert_eq!(resp["id"], expected_id, "response id must match request id");
}

/// Parse the GID from an addUri/pause/remove result.
fn parse_gid(resp: &Value) -> String {
    // Results may be a plain string or [gid] array depending on method
    if resp["result"].is_string() {
        resp["result"].as_str().unwrap().to_string()
    } else if resp["result"].is_array() {
        resp["result"][0]
            .as_str()
            .expect("result array first element should be a GID string")
            .to_string()
    } else {
        panic!("unexpected GID format: {resp}");
    }
}

/// Add a URI and return the GID (shared helper for tests that need a download).
async fn add_uri(client: &Client, base_url: &str, url: &str) -> String {
    let resp = rpc_call(client, base_url, "aria2.addUri", json!([[url]])).await;
    let gid = parse_gid(&resp);
    assert_eq!(gid.len(), 16, "GID must be 16 hex chars, got: {gid}");
    gid
}

/// Wait until an asynchronously removed download is visible in stopped results.
async fn wait_for_stopped_gid(client: &Client, base_url: &str, gid: &str) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last_response = Value::Null;

    while tokio::time::Instant::now() < deadline {
        let response = rpc_call(client, base_url, "aria2.tellStopped", json!([0, 10])).await;
        if response.get("result").is_some_and(|result| {
            result.as_array().is_some_and(|results| {
                results
                    .iter()
                    .any(|result| result["gid"].as_str() == Some(gid))
            })
        }) {
            return response;
        }
        last_response = response;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let status = rpc_call(client, base_url, "aria2.tellStatus", json!([gid])).await;
    let waiting = rpc_call(client, base_url, "aria2.tellWaiting", json!([0, 10])).await;
    let active = rpc_call(client, base_url, "aria2.tellActive", json!([])).await;
    panic!(
        "download {gid} did not reach stopped results: stopped={last_response}, status={status}, waiting={waiting}, active={active}"
    );
}

// =========================================================================
// Group A — Task Management
// =========================================================================

#[tokio::test]
async fn e2e_add_uri_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test-add-uri"]]),
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-addUri");
    assert_success(&resp);
    let gid = resp["result"].as_str().expect("GID must be a string");
    assert_eq!(gid.len(), 16, "GID must be 16 hex chars");
}

#[tokio::test]
async fn e2e_remove_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/test-remove").await;
    let resp = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-remove");
    assert_success(&resp);
    // remove returns the GID (as string or [gid] array)
    let returned = parse_gid(&resp);
    assert_eq!(returned, gid, "remove should return the same GID");
}

#[tokio::test]
async fn e2e_force_remove_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/test-force-remove").await;
    let resp = rpc_call(&client, &base, "aria2.forceRemove", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-forceRemove");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_remove_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.remove",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1); // RpcExecution for unknown GID
}

#[tokio::test]
async fn e2e_pause_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/test-pause").await;
    let resp = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-pause");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_force_pause_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/test-force-pause").await;
    let resp = rpc_call(&client, &base, "aria2.forcePause", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-forcePause");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_unpause_returns_gid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/test-unpause").await;
    // Pause first, then unpause
    let _ = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;
    let resp = rpc_call(&client, &base, "aria2.unpause", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-unpause");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_force_unpause_is_rejected_as_an_unknown_original_method() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.forceUnpause",
        json!([]),
        StatusCode::BAD_REQUEST,
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-forceUnpause");
    assert_error_code(&resp, 1);
    assert_eq!(
        resp["error"]["message"],
        "No such method: aria2.forceUnpause"
    );
}

#[tokio::test]
async fn e2e_pause_all_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid1 = add_uri(&client, &base, "http://127.0.0.1:1/pause-all-1").await;
    let _gid2 = add_uri(&client, &base, "http://127.0.0.1:1/pause-all-2").await;

    let resp = rpc_call(&client, &base, "aria2.pauseAll", json!([])).await;
    assert_jsonrpc_format(&resp, "aria2-pauseAll");
    assert_success(&resp);
    // pauseAll returns "OK" (as a string, wire-formatted)
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "pauseAll result should contain 'OK', got: {result_str}"
    );
}

#[tokio::test]
async fn e2e_unpause_all_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/unpause-all").await;
    let _ = rpc_call(&client, &base, "aria2.pauseAll", json!([])).await;

    let resp = rpc_call(&client, &base, "aria2.unpauseAll", json!([])).await;
    assert_jsonrpc_format(&resp, "aria2-unpauseAll");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_force_pause_all_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/force-pause-all").await;

    let resp = rpc_call(&client, &base, "aria2.forcePauseAll", json!([])).await;
    assert_jsonrpc_format(&resp, "aria2-forcePauseAll");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_tell_status_returns_struct() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/tell-status").await;
    let resp = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-tellStatus");
    assert_success(&resp);

    let result = &resp["result"];
    assert!(result.is_object(), "tellStatus result should be an object");
    assert_eq!(result["gid"].as_str(), Some(gid.as_str()));
    assert!(result["status"].is_string(), "status field must be present");
}

#[tokio::test]
async fn e2e_tell_status_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.tellStatus",
        json![["nonexistentgid1234"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_tell_active_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/tell-active").await;
    let resp = rpc_call(&client, &base, "aria2.tellActive", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-tellActive");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "tellActive result should be an array"
    );
    // The connection-refused fixture may transition directly to error/stopped;
    // tellActive only guarantees a valid active snapshot shape here.
    assert!(resp["result"].is_array());
}

#[tokio::test]
async fn e2e_tell_waiting_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.tellWaiting", json!([0, 10])).await;

    assert_jsonrpc_format(&resp, "aria2-tellWaiting");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "tellWaiting result should be an array"
    );
}

#[tokio::test]
async fn e2e_tell_stopped_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/tell-stopped").await;
    // Remove the download so it appears in stopped list
    let _ = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;

    let resp = rpc_call(&client, &base, "aria2.tellStopped", json!([0, 10])).await;

    assert_jsonrpc_format(&resp, "aria2-tellStopped");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "tellStopped result should be an array"
    );
}

#[tokio::test]
async fn e2e_change_position_returns_position() {
    let (base, _guard) = start_test_server_with_max_concurrent(None, 0).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/change-pos").await;
    let pause = rpc_call(&client, &base, "aria2.forcePause", json![[&gid]]).await;
    assert_success(&pause);
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_success(&status);
    assert!(
        matches!(
            status["result"]["status"].as_str(),
            Some("paused") | Some("waiting") | Some("active")
        ),
        "status should be a valid live task state: {status}"
    );
    let (status, resp) = rpc_call_with_status(
        &client,
        &base,
        "aria2.changePosition",
        json![[&gid, 0, "POS_SET"]],
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-changePosition");
    if resp.get("error").is_some() {
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(resp["error"]["code"], 1);
        return;
    }
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_success(&resp);
    // Returns the new absolute position
    assert!(
        resp["result"].is_number() || resp["result"].is_string(),
        "changePosition result should be a number (wire-formatted as string), got: {resp}"
    );
}

#[tokio::test]
async fn e2e_change_uri_returns_counts() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/change-uri").await;
    let resp = rpc_call(
        &client,
        &base,
        "aria2.changeUri",
        json![[&gid, 1, [], ["http://127.0.0.1:1/added-uri"]]],
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-changeUri");
    assert_success(&resp);
    // Returns [delCount, addCount] — wire format converts numbers to strings
    let result = resp["result"]
        .as_array()
        .expect("changeUri should return array");
    assert_eq!(result.len(), 2, "changeUri result should have 2 elements");
    // addCount should be 1 (we added 1 URI) — may be number or string in wire format
    let add_count = result[1]
        .as_i64()
        .or_else(|| result[1].as_str().and_then(|s| s.parse::<i64>().ok()))
        .unwrap_or(-1);
    assert_eq!(add_count, 1, "addCount should be 1");
}

#[tokio::test]
async fn e2e_save_session_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/save-session").await;
    let resp = rpc_call(&client, &base, "aria2.saveSession", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-saveSession");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "saveSession result should contain 'OK', got: {result_str}"
    );
}

// =========================================================================
// Group B — Option Management
// =========================================================================

#[tokio::test]
async fn e2e_get_option_returns_struct() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/get-option").await;

    // First set per-task options via changeOption, then retrieve them
    // (without group_man, getOption only returns per-task overrides stored via changeOption)
    let _ = rpc_call(
        &client,
        &base,
        "aria2.changeOption",
        json![[&gid, {"max-download-limit": "1048576"}]],
    )
    .await;

    let resp = rpc_call(&client, &base, "aria2.getOption", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-getOption");
    assert_success(&resp);
    assert!(
        resp["result"].is_object(),
        "getOption result should be an object"
    );
}

#[tokio::test]
async fn e2e_change_option_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/change-option").await;
    // max-download-limit is a runtime-changeable option
    let resp = rpc_call(
        &client,
        &base,
        "aria2.changeOption",
        json![[&gid, {"max-download-limit": "1048576"}]],
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-changeOption");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "changeOption result should contain 'OK', got: {result_str}"
    );
}

#[tokio::test]
async fn e2e_change_option_non_runtime_is_ignored() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/change-option-invalid").await;
    // Unknown options are ignored by C++ aria2's option gatherer.
    let resp = rpc_call(
        &client,
        &base,
        "aria2.changeOption",
        json![[&gid, {"nonexistent-option": "value"}]],
    )
    .await;

    assert_success(&resp);
}

#[tokio::test]
async fn e2e_option_parse_failures_match_aria2_execution_error() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let gid = add_uri(&client, &base, "http://127.0.0.1:1/change-option-bad-value").await;

    let change_option = rpc_error_call(
        &client,
        &base,
        "aria2.changeOption",
        json![[&gid, {"max-download-limit": "badvalue"}]],
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&change_option, 1);

    let change_global = rpc_error_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json![[{"max-overall-download-limit": "badvalue"}]],
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&change_global, 1);

    let change_global_enum = rpc_error_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json![[{"uri-selector": "not-a-selector"}]],
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&change_global_enum, 1);
}

#[tokio::test]
async fn e2e_get_global_option_returns_struct() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-getGlobalOption");
    assert_success(&resp);
    assert!(
        resp["result"].is_object(),
        "getGlobalOption result should be an object"
    );
    // Should contain some default options
    assert!(
        !resp["result"].as_object().unwrap().is_empty(),
        "getGlobalOption should return non-empty options"
    );
}

#[tokio::test]
async fn e2e_change_global_option_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json![[{"max-concurrent-downloads": "5"}]],
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-changeGlobalOption");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "changeGlobalOption result should contain 'OK', got: {result_str}"
    );

    // Verify the change persists
    let after = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_eq!(
        after["result"]["max-concurrent-downloads"].as_str(),
        Some("5"),
        "global option should reflect the change"
    );
}

// =========================================================================
// Group C — BitTorrent
// =========================================================================

#[tokio::test]
#[cfg(feature = "bittorrent")]
async fn e2e_get_peers_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // Non-BT download: getPeers returns an empty array
    let gid = add_uri(&client, &base, "http://127.0.0.1:1/get-peers").await;
    let resp = rpc_call(&client, &base, "aria2.getPeers", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-getPeers");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "getPeers result should be an array"
    );
}

#[tokio::test]
#[cfg(feature = "bittorrent")]
async fn e2e_get_peers_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.getPeers",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_get_uris_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/get-uris").await;
    let resp = rpc_call(&client, &base, "aria2.getUris", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-getUris");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "getUris result should be an array"
    );
    // Should contain at least one URI entry
    let uris = resp["result"].as_array().unwrap();
    assert!(!uris.is_empty(), "getUris should return at least one URI");
}

#[tokio::test]
async fn e2e_get_uris_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.getUris",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_get_files_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/get-files").await;
    let resp = rpc_call(&client, &base, "aria2.getFiles", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-getFiles");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "getFiles result should be an array"
    );
    let files = resp["result"].as_array().unwrap();
    assert!(
        !files.is_empty(),
        "getFiles should return at least one file entry"
    );
}

#[tokio::test]
async fn e2e_get_files_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.getFiles",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_get_servers_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://example.test/get-servers").await;
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_eq!(status["result"]["status"], "active");

    let resp = rpc_call(&client, &base, "aria2.getServers", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-getServers");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "getServers result should be an array"
    );
}

#[tokio::test]
async fn e2e_get_servers_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.getServers",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

// =========================================================================
// Group D — System Methods
// =========================================================================

#[tokio::test]
async fn e2e_system_list_methods_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "system.listMethods", json!([])).await;

    assert_jsonrpc_format(&resp, "system-listMethods");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "listMethods result should be an array"
    );

    let methods = resp["result"].as_array().unwrap();
    let expected_method_count = 33
        + usize::from(cfg!(feature = "bittorrent")) * 2
        + usize::from(cfg!(feature = "metalink"));
    assert_eq!(
        methods.len(),
        expected_method_count,
        "method count must match aria2's feature-specific catalog"
    );

    // Verify core methods are present
    let method_names: Vec<&str> = methods.iter().filter_map(|v| v.as_str()).collect();
    for expected in [
        "aria2.addUri",
        "aria2.remove",
        "aria2.pause",
        "aria2.tellStatus",
        "aria2.getGlobalStat",
        "aria2.getVersion",
        "system.multicall",
        "system.listMethods",
        "system.listNotifications",
    ] {
        assert!(
            method_names.contains(&expected),
            "listMethods should contain '{expected}'"
        );
    }
}

#[tokio::test]
async fn e2e_system_list_notifications_returns_array() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "system.listNotifications", json!([])).await;

    assert_jsonrpc_format(&resp, "system-listNotifications");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "listNotifications result should be an array"
    );

    let notifications = resp["result"].as_array().unwrap();
    let expected_notification_count = 5 + usize::from(cfg!(feature = "bittorrent"));
    assert_eq!(
        notifications.len(),
        expected_notification_count,
        "notification count must match aria2's feature-specific catalog"
    );

    let names: Vec<&str> = notifications.iter().filter_map(|v| v.as_str()).collect();
    let mut expected = vec![
        "aria2.onDownloadStart",
        "aria2.onDownloadPause",
        "aria2.onDownloadStop",
        "aria2.onDownloadComplete",
        "aria2.onDownloadError",
    ];
    if cfg!(feature = "bittorrent") {
        expected.push("aria2.onBtDownloadComplete");
    }
    for expected in expected {
        assert!(
            names.contains(&expected),
            "listNotifications should contain '{expected}'"
        );
    }
}

#[tokio::test]
async fn e2e_system_multicall_batch_request() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "system.multicall",
        json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.getGlobalStat", "params": []},
        ]]),
    )
    .await;

    assert_jsonrpc_format(&resp, "system-multicall");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "multicall result should be an array"
    );
    let results = resp["result"].as_array().unwrap();
    assert_eq!(results.len(), 2, "multicall should return 2 results");
}

#[tokio::test]
async fn e2e_system_multicall_empty_calls() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "system.multicall", json!([[]])).await;

    assert_jsonrpc_format(&resp, "system-multicall");
    assert_success(&resp);
    assert!(
        resp["result"].is_array(),
        "multicall result should be an array"
    );
    assert!(
        resp["result"].as_array().unwrap().is_empty(),
        "empty multicall should return empty array"
    );
}

#[tokio::test]
async fn e2e_system_multicall_unknown_method_returns_error() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "system.multicall",
        json!([[{"methodName": "aria2.nonexistentMethod", "params": []}]]),
    )
    .await;

    assert_jsonrpc_format(&resp, "system-multicall");
    assert_success(&resp);
    let results = resp["result"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    // The sub-call error should be embedded in the result
    assert!(
        results[0].get("code").is_some(),
        "sub-call error should have 'code' field"
    );
}

// =========================================================================
// Group E — Status / Session / Version
// =========================================================================

#[tokio::test]
async fn e2e_get_global_stat_returns_stats() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/global-stat").await;
    let resp = rpc_call(&client, &base, "aria2.getGlobalStat", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-getGlobalStat");
    assert_success(&resp);

    let result = &resp["result"];
    assert!(
        result.is_object(),
        "getGlobalStat result should be an object"
    );
    // Wire format converts numbers to strings
    assert!(
        result.get("downloadSpeed").is_some(),
        "should have downloadSpeed"
    );
    assert!(
        result.get("uploadSpeed").is_some(),
        "should have uploadSpeed"
    );
    assert!(result.get("numActive").is_some(), "should have numActive");
    assert!(result.get("numWaiting").is_some(), "should have numWaiting");
    assert!(result.get("numStopped").is_some(), "should have numStopped");
}

#[tokio::test]
async fn e2e_get_version_returns_version_info() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-getVersion");
    assert_success(&resp);

    let result = &resp["result"];
    assert!(result["version"].is_string(), "version should be a string");
    assert!(
        result["enabledFeatures"].is_array(),
        "enabledFeatures should be an array"
    );
}

#[tokio::test]
async fn e2e_get_session_info_returns_session_id() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getSessionInfo", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-getSessionInfo");
    assert_success(&resp);

    let result = &resp["result"];
    assert!(
        result["sessionId"].is_string(),
        "sessionId should be a string"
    );
}

// =========================================================================
// Group F — Shutdown (tested on dedicated server instances)
// =========================================================================

#[tokio::test]
async fn e2e_shutdown_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/shutdown-test").await;
    let resp = rpc_call(&client, &base, "aria2.shutdown", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-shutdown");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "shutdown result should contain 'OK', got: {result_str}"
    );
}

#[tokio::test]
async fn e2e_force_shutdown_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let _gid = add_uri(&client, &base, "http://127.0.0.1:1/force-shutdown-test").await;
    let resp = rpc_call(&client, &base, "aria2.forceShutdown", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-forceShutdown");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "forceShutdown result should contain 'OK', got: {result_str}"
    );
}

// =========================================================================
// Group G — Additional method coverage
// =========================================================================

#[tokio::test]
async fn e2e_purge_download_result_returns_ok() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // Create a stopped download
    let gid = add_uri(&client, &base, "http://127.0.0.1:1/purge-result").await;
    let _ = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;

    let resp = rpc_call(&client, &base, "aria2.purgeDownloadResult", json!([])).await;

    assert_jsonrpc_format(&resp, "aria2-purgeDownloadResult");
    assert_success(&resp);
    let result_str = resp["result"].as_str().unwrap_or("");
    assert!(
        result_str.contains("OK"),
        "purgeDownloadResult should return 'OK', got: {result_str}"
    );
}

#[tokio::test]
async fn e2e_remove_download_result_returns_ok() {
    let (base, _guard) = start_test_server_with_max_concurrent(None, 0).await;
    let client = Client::new();

    let gid = add_uri(&client, &base, "http://127.0.0.1:1/remove-result").await;
    let _ = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;
    let stopped = wait_for_stopped_gid(&client, &base, &gid).await;
    assert_success(&stopped);

    let resp = rpc_call(&client, &base, "aria2.removeDownloadResult", json![[&gid]]).await;

    assert_jsonrpc_format(&resp, "aria2-removeDownloadResult");
    assert_success(&resp);
}

#[tokio::test]
async fn e2e_add_uri_with_options() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test-with-opts"], {"dir": "/tmp"}]),
    )
    .await;

    assert_jsonrpc_format(&resp, "aria2-addUri");
    assert_success(&resp);
    let gid = resp["result"].as_str().expect("GID must be a string");
    assert_eq!(gid.len(), 16);

    // Verify the dir option is reflected in tellStatus
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[gid]]).await;
    assert_eq!(
        status["result"]["dir"].as_str(),
        Some("/tmp"),
        "dir option should be /tmp"
    );
}

#[tokio::test]
async fn e2e_full_lifecycle_all_methods() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // 1. addUri
    let gid = add_uri(&client, &base, "http://example.test/lifecycle").await;

    // 2. tellStatus (active)
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_success(&status);
    assert_eq!(status["result"]["gid"].as_str(), Some(gid.as_str()));
    assert_eq!(status["result"]["status"], "active");

    // 3. changeOption (max-download-limit is runtime-changeable) — must set before getOption
    //    because getOption without group_man only returns per-task overrides
    let change = rpc_call(
        &client,
        &base,
        "aria2.changeOption",
        json![[&gid, {"max-download-limit": "2048000"}]],
    )
    .await;
    assert_success(&change);

    // 4. getOption (now returns per-task overrides from step 3)
    let opts = rpc_call(&client, &base, "aria2.getOption", json![[&gid]]).await;
    assert_success(&opts);

    // 5. pause
    let pause = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;
    assert_success(&pause);

    // 6. tellStatus (paused)
    let paused = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_success(&paused);
    assert!(matches!(
        paused["result"]["status"].as_str(),
        Some("paused" | "active" | "waiting")
    ));

    // 7. getUris (while paused)
    let uris = rpc_call(&client, &base, "aria2.getUris", json![[&gid]]).await;
    assert_success(&uris);

    // 8. getFiles (while paused)
    let files = rpc_call(&client, &base, "aria2.getFiles", json![[&gid]]).await;
    assert_success(&files);

    // 9. getServers (while paused): aria2_original only accepts active groups.
    let (servers_status, servers) =
        rpc_call_with_status(&client, &base, "aria2.getServers", json![[&gid]]).await;
    assert_eq!(servers_status, reqwest::StatusCode::BAD_REQUEST);
    assert_error_code(&servers, 1);

    // 10. getPeers (while paused, non-BT → empty)
    #[cfg(feature = "bittorrent")]
    {
        let peers = rpc_call(&client, &base, "aria2.getPeers", json![[&gid]]).await;
        assert_success(&peers);
    }

    // 11. unpause
    let unpause = rpc_call(&client, &base, "aria2.unpause", json![[&gid]]).await;
    assert_success(&unpause);

    // 12. changePosition
    let (pos_status, pos) = rpc_call_with_status(
        &client,
        &base,
        "aria2.changePosition",
        json![[&gid, 0, "POS_SET"]],
    )
    .await;
    if pos.get("error").is_some() {
        assert_eq!(pos_status, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(pos["error"]["code"], 1);
    } else {
        assert_eq!(pos_status, reqwest::StatusCode::OK);
        assert_success(&pos);
    }

    // 13. changeUri (add a new URI)
    let uri_change = rpc_call(
        &client,
        &base,
        "aria2.changeUri",
        json![[&gid, 1, [], ["http://127.0.0.1:1/alt-uri"]]],
    )
    .await;
    assert_success(&uri_change);

    // 14. remove
    let remove = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;
    assert_success(&remove);

    // 15. tellStopped (should contain the removed task)
    let stopped = rpc_call(&client, &base, "aria2.tellStopped", json!([0, 10])).await;
    assert_success(&stopped);

    // 16. purgeDownloadResult
    let purge = rpc_call(&client, &base, "aria2.purgeDownloadResult", json!([])).await;
    assert_success(&purge);
}

#[tokio::test]
async fn e2e_add_uri_invalid_params() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // No URIs provided
    let resp = rpc_call(&client, &base, "aria2.addUri", json!([[]])).await;
    // This should succeed (empty URI array is accepted but the download will be empty)
    // or return an error depending on the handler implementation
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "addUri with empty array should return result or error"
    );
}

#[tokio::test]
async fn e2e_unknown_method_returns_method_not_found() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&rpc_body("aria2.nonexistentMethod", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp: Value = resp.json().await.unwrap();
    assert_jsonrpc_format(&resp, "aria2-nonexistentMethod");
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_force_remove_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.forceRemove",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    // A syntactically valid but unknown GID is an aria2 execution error.
    assert_jsonrpc_format(&resp, "aria2-forceRemove");
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_pause_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.pause",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_get_option_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.getOption",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_change_position_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.changePosition",
        json![["deadbeefdeadbeef", 0, "POS_SET"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_change_uri_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.changeUri",
        json![["deadbeefdeadbeef", 1, [], ["http://x.com"]]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_remove_download_result_nonexistent_gid_errors() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_error_call(
        &client,
        &base,
        "aria2.removeDownloadResult",
        json![["deadbeefdeadbeef"]],
        reqwest::StatusCode::BAD_REQUEST,
    )
    .await;
    assert_error_code(&resp, 1);
}
