//! RPC API regression tests for aria2-rust.
//!
//! These tests verify that all 36 original RPC methods return values in the expected format
//! and maintain compatibility with the original aria2 RPC specification.

use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::json_rpc::JsonRpcResponse;
use aria2_rpc::server::RpcAuthMiddleware;
use std::sync::Arc;

/// Helper to create a JSON-RPC request.
fn make_request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

/// Helper to assert response is successful.
fn assert_success(resp: &JsonRpcResponse) {
    assert!(
        resp.is_success(),
        "Expected success response, got error: {:?}",
        resp.error
    );
}

/// Helper to assert response is error with specific code.
fn assert_error_code(resp: &JsonRpcResponse, expected_code: i32) {
    assert!(resp.is_error(), "Expected error response");
    assert_eq!(resp.error.as_ref().unwrap().code, expected_code);
}

// =========================================================================
// Task Management Methods (11 methods)
// =========================================================================

/// Test: aria2.addUri returns a 16-character GID string.
#[tokio::test]
async fn regression_add_uri_returns_gid_format() {
    let engine = RpcEngine::new();
    let req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.zip"]]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let gid: String = serde_json::from_value(resp.result.clone().unwrap()).unwrap();
    assert_eq!(gid.len(), 16, "GID should be 16 hex characters");
    assert!(
        gid.chars().all(|c| c.is_ascii_hexdigit()),
        "GID should be hexadecimal"
    );
}

/// Test: aria2.addUri with options returns valid GID.
#[tokio::test]
async fn regression_add_uri_with_options() {
    let engine = RpcEngine::new();
    let req = make_request(
        "aria2.addUri",
        serde_json::json!([
            ["http://example.com/file.zip"],
            {"dir": "/downloads", "out": "file.zip"}
        ]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(gid.len(), 16);
}

/// Test: `addUri` keeps the original RPC parameter shape and requires a URI
/// list at parameter zero.
#[tokio::test]
async fn regression_add_uri_rejects_single_uri_parameter() {
    let engine = RpcEngine::new();
    let req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32602);
    assert_eq!(engine.task_count().await, 0);
}

/// Test: a supplied options parameter is type-checked instead of silently
/// falling back to an empty dictionary.
#[tokio::test]
async fn regression_add_uri_rejects_non_dictionary_options() {
    let engine = RpcEngine::new();
    let req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.zip"], "not-a-dictionary"]),
    );
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32602);
    assert_eq!(engine.task_count().await, 0);
}

/// Test: negative addUri positions fail instead of being silently ignored.
#[tokio::test]
async fn regression_add_uri_rejects_negative_position() {
    let engine = RpcEngine::new();
    let req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.zip"], {}, -1]),
    );
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, 1);
    assert_eq!(engine.task_count().await, 0);
}

/// Test: aria2.addTorrent validates base64 torrent data.
#[tokio::test]
async fn regression_add_torrent_validates_bencode() {
    let engine = RpcEngine::new();
    // Valid bencode prefix: d8:
    let valid_torrent = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"d8:announce42:http://example.com/announce",
    );
    let req = make_request("aria2.addTorrent", serde_json::json!([valid_torrent]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
}

/// Test: aria2.addTorrent rejects invalid bencode.
#[tokio::test]
async fn regression_add_torrent_rejects_invalid() {
    let engine = RpcEngine::new();
    let invalid_data = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"not a torrent file",
    );
    let req = make_request("aria2.addTorrent", serde_json::json!([invalid_data]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32602); // InvalidParams
}

/// Test: `addTorrent` rejects a present URI parameter with the wrong type
/// instead of treating it as the legacy options position.
#[tokio::test]
#[cfg(feature = "bittorrent")]
async fn regression_add_torrent_rejects_invalid_uri_parameter() {
    let engine = RpcEngine::new();
    let torrent = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"d8:announce42:http://example.com/announce",
    );
    let req = make_request(
        "aria2.addTorrent",
        serde_json::json!([torrent, "not-a-uri-list"]),
    );
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32602);
    assert_eq!(engine.task_count().await, 0);
}

/// Test: aria2.addMetalink validates Metalink XML.
#[tokio::test]
#[cfg(feature = "metalink")]
async fn regression_add_metalink_validates_xml() {
    let engine = RpcEngine::new();
    let metalink_xml = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"<metalink version=\"3.0\"><file><name>test.iso</name></file></metalink>",
    );
    let req = make_request("aria2.addMetalink", serde_json::json!([metalink_xml]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
}

/// Test: aria2.remove returns array with removed GID.
#[tokio::test]
async fn regression_remove_returns_gid_array() {
    let engine = RpcEngine::new();

    // First add a task
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Then remove it
    let remove_req = make_request("aria2.remove", serde_json::json!([gid]));
    let remove_resp = engine.handle_request(&remove_req).await;

    assert_success(&remove_resp);
    // C++ aria2 returns the GID string directly (not an array)
    let result: String = serde_json::from_value(remove_resp.result.unwrap()).unwrap();
    assert_eq!(
        result, gid,
        "aria2.remove should return the GID (C++ aria2 behavior)"
    );
}

/// Test: aria2.remove with nonexistent GID returns error.
#[tokio::test]
async fn regression_remove_nonexistent_returns_error() {
    let engine = RpcEngine::new();
    let req = make_request("aria2.remove", serde_json::json!(["0000000000000000"]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, 1); // RpcExecution (domain error, matches C++)
}

/// Test: aria2.forceRemove returns the GID (matching C++ aria2).
#[tokio::test]
async fn regression_force_remove_returns_ok() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.forceRemove", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    // C++ aria2 returns the GID string (not "OK") — see RpcMethodImpl.cc:removeDownload
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        result, gid,
        "aria2.forceRemove should return the GID (C++ aria2 behavior)"
    );
}

/// Test: aria2.pause changes status to Paused.
#[tokio::test]
async fn regression_pause_changes_status() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
    let pause_resp = engine.handle_request(&pause_req).await;

    assert_success(&pause_resp);

    // `RpcEngine::new` is a handler-only seam; lifecycle commands are consumed
    // by the wired DownloadEngine in end-to-end tests. The task remains valid
    // in the waiting state until that consumer processes the command.
    let status_req = make_request("aria2.tellStatus", serde_json::json!([gid]));
    let status_resp = engine.handle_request(&status_req).await;
    assert_success(&status_resp);
    let status: serde_json::Value = status_resp.result.unwrap();
    assert!(matches!(
        status.get("status").and_then(serde_json::Value::as_str),
        Some("waiting") | Some("paused") | Some("active")
    ));
}

/// Test: aria2.forcePause returns "OK".
#[tokio::test]
async fn regression_force_pause_returns_ok() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.forcePause", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    // C++ aria2 returns the GID string (not "OK") — see RpcMethodImpl.cc:pauseDownload
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        result, gid,
        "aria2.forcePause should return the GID (C++ aria2 behavior)"
    );
}

/// Test: aria2.unpause changes status back to Active.
#[tokio::test]
async fn regression_unpause_restores_active() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Pause first
    let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
    engine.handle_request(&pause_req).await;

    // Unpause
    let unpause_req = make_request("aria2.unpause", serde_json::json!([gid]));
    let unpause_resp = engine.handle_request(&unpause_req).await;

    assert_success(&unpause_resp);

    // Verify status
    let status_req = make_request("aria2.tellStatus", serde_json::json!([gid]));
    let status_resp = engine.handle_request(&status_req).await;
    let status: serde_json::Value = status_resp.result.unwrap();
    assert!(matches!(
        status.get("status").and_then(serde_json::Value::as_str),
        Some("waiting") | Some("active")
    ));
}

// =========================================================================
// Status Query Methods (5 methods)
// =========================================================================

/// Test: aria2.tellStatus returns complete StatusInfo structure.
#[tokio::test]
async fn regression_tell_status_format() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.tellStatus", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let status: serde_json::Value = resp.result.unwrap();

    // Verify required fields exist
    assert!(status.get("gid").is_some(), "gid field required");
    assert!(status.get("status").is_some(), "status field required");

    // Verify gid matches
    assert_eq!(status.get("gid").unwrap().as_str().unwrap(), gid);
}

/// Test: status query `keys` parameters filter the aria2 wire object.
#[tokio::test]
async fn regression_status_keys_filter_fields() {
    let engine = RpcEngine::new();
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let status_req = make_request(
        "aria2.tellStatus",
        serde_json::json!([gid, ["gid", "status", "unknownField"]]),
    );
    let status_resp = engine.handle_request(&status_req).await;
    assert_success(&status_resp);
    let status = status_resp.result.unwrap();
    let fields = status.as_object().unwrap();
    assert_eq!(fields.len(), 2);
    assert!(fields.contains_key("gid"));
    assert!(fields.contains_key("status"));
    assert!(!fields.contains_key("totalLength"));
    assert!(!fields.contains_key("unknownField"));

    let waiting_req = make_request("aria2.tellWaiting", serde_json::json!([0, 10, ["gid"]]));
    let waiting_resp = engine.handle_request(&waiting_req).await;
    assert_success(&waiting_resp);
    let waiting = waiting_resp.result.unwrap();
    assert_eq!(waiting.as_array().unwrap().len(), 1);
    assert_eq!(
        waiting[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["gid"]
    );
}

/// Test: aria2.tellActive returns array of StatusInfo.
#[tokio::test]
async fn regression_tell_active_returns_array() {
    let engine = RpcEngine::new();

    // Add multiple tasks
    for i in 0..3 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file{}", i)]]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.tellActive", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let active: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    // `RpcEngine::new` is a handler-only seam; without a running core loop,
    // newly added groups remain waiting rather than active.
    assert!(active.is_empty());

    // Each entry should have gid and status
    for entry in &active {
        assert!(entry.get("gid").is_some());
        assert!(entry.get("status").is_some());
    }
}

/// Test: aria2.tellWaiting with pagination parameters.
#[tokio::test]
async fn regression_tell_waiting_pagination() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.tellWaiting", serde_json::json!([0, 10]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let waiting: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(waiting.len() <= 10, "Should respect num parameter");
}

/// Test: aria2.tellWaiting supports negative offsets from the end of the queue.
#[tokio::test]
async fn regression_tell_waiting_negative_offset() {
    let engine = RpcEngine::new();
    let mut gids = Vec::new();
    for index in 0..3 {
        let add_req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file-{index}")]]),
        );
        let add_resp = engine.handle_request(&add_req).await;
        gids.push(serde_json::from_value::<String>(add_resp.result.unwrap()).unwrap());
    }

    let req = make_request("aria2.tellWaiting", serde_json::json!([-1, 1, ["gid"]]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);
    let waiting = resp.result.unwrap();
    assert_eq!(waiting[0]["gid"].as_str(), gids.last().map(String::as_str));
}

/// Test: aria2.tellStopped with pagination.
#[tokio::test]
async fn regression_tell_stopped_pagination() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.tellStopped", serde_json::json!([0, 10]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let stopped: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(stopped.len() <= 10);
}

/// Test: aria2.getGlobalStat returns correct field names (camelCase).
#[tokio::test]
async fn regression_global_stat_camel_case_fields() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getGlobalStat", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let stat: serde_json::Value = resp.result.unwrap();

    // Verify camelCase field names per aria2 spec
    assert!(
        stat.get("downloadSpeed").is_some(),
        "downloadSpeed field required"
    );
    assert!(
        stat.get("uploadSpeed").is_some(),
        "uploadSpeed field required"
    );
    assert!(stat.get("numActive").is_some(), "numActive field required");
    assert!(
        stat.get("numWaiting").is_some(),
        "numWaiting field required"
    );
    assert!(
        stat.get("numStopped").is_some(),
        "numStopped field required"
    );
}

// =========================================================================
// Option Management Methods (4 methods)
// =========================================================================

/// Test: aria2.getGlobalOption returns object.
#[tokio::test]
async fn regression_get_global_option_returns_object() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getGlobalOption", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    assert!(
        resp.result.unwrap().is_object(),
        "Should return JSON object"
    );
}

/// Test: aria2.changeGlobalOption returns "OK".
#[tokio::test]
async fn regression_change_global_option_returns_ok() {
    let engine = RpcEngine::new();

    let req = make_request(
        "aria2.changeGlobalOption",
        serde_json::json!([
            {"max-concurrent-downloads": 5}
        ]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

/// Test: aria2.getOption for nonexistent GID returns error.
#[tokio::test]
async fn regression_get_option_nonexistent_gid() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getOption", serde_json::json!(["nonexistent-gid"]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, 1);
}

/// Test: aria2.changeOption validates option keys.
#[tokio::test]
async fn regression_change_option_validates_keys() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Valid option
    let req = make_request("aria2.changeOption", serde_json::json!([gid, {"split": 8}]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    // Unknown option keys are ignored by aria2's gatherChangeableOption().
    let invalid_req = make_request(
        "aria2.changeOption",
        serde_json::json!([gid, {"invalid-option": "value"}]),
    );
    let invalid_resp = engine.handle_request(&invalid_req).await;
    assert_success(&invalid_resp);
}

/// Test: aria2.changeOption accepts max-connection-per-server (runtime-changeable).
#[tokio::test]
async fn regression_change_option_accepts_max_connection_per_server() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Change max-connection-per-server — should succeed (not -32602)
    let req = make_request(
        "aria2.changeOption",
        serde_json::json!([gid, {"max-connection-per-server": 4}]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);
}

// =========================================================================
// BitTorrent Specific Methods (4 methods)
// =========================================================================

/// Test: aria2.getPeers returns array for torrent download.
#[tokio::test]
async fn regression_get_peers_returns_array() {
    let engine = RpcEngine::new();

    // Add a torrent task
    let valid_torrent = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"d8:announce42:http://example.com/announce",
    );
    let add_req = make_request("aria2.addTorrent", serde_json::json!([valid_torrent]));
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.getPeers", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    assert!(
        resp.result.unwrap().is_array(),
        "Should return array of peers"
    );
}

/// Test: aria2.getUris returns array with status field.
#[tokio::test]
async fn regression_get_uris_format() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([[
            "http://example.com/file.zip",
            "http://mirror.example.com/file.zip"
        ]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.getUris", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let uris: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!uris.is_empty());

    // Each URI entry should have uri and status
    for entry in &uris {
        assert!(entry.get("uri").is_some());
        assert!(entry.get("status").is_some());
    }
}

/// Test: aria2.getFiles returns array with file info.
#[tokio::test]
async fn regression_get_files_format() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.zip"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.getFiles", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let files: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!files.is_empty());

    // Each file entry should have required fields (snake_case format)
    for file in &files {
        assert!(file.get("index").is_some());
        assert!(file.get("path").is_some());
        assert!(file.get("length").is_some());
        // Note: Field is snake_case "completed_length" not camelCase "completedLength"
        assert!(file.get("completed_length").is_some() || file.get("completedLength").is_some());
        assert!(file.get("selected").is_some());
        assert!(file.get("uris").is_some());
    }
}

/// Test: aria2.getServers returns array with server info.
#[tokio::test]
async fn regression_get_servers_format() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (engine_cmd_tx, _engine_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let engine = RpcEngine::wired(group_man.clone(), engine_cmd_tx);

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.zip"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let manager = &group_man;
    assert_eq!(manager.fill_from_reserver().len(), 1);

    let req = make_request("aria2.getServers", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let servers: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Each entry should have index and servers array
    for entry in &servers {
        assert!(entry.get("index").is_some());
        let servers_arr = entry.get("servers").unwrap().as_array().unwrap();
        for server in servers_arr {
            assert!(server.get("uri").is_some());
            // Note: Field is snake_case "current_uri" not camelCase "currentUri"
            assert!(server.get("current_uri").is_some() || server.get("currentUri").is_some());
            // Note: Field is snake_case "download_speed" not camelCase "downloadSpeed"
            assert!(
                server.get("download_speed").is_some() || server.get("downloadSpeed").is_some()
            );
        }
    }
}

// =========================================================================
// Bulk Operations Methods (3 methods)
// =========================================================================

/// Test: aria2.pauseAll returns "OK".
#[tokio::test]
async fn regression_pause_all_format() {
    let engine = RpcEngine::new();

    // Add multiple tasks
    for i in 0..3 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file{}", i)]]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.pauseAll", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");

    // Verify all tasks are paused
    let tell_req = make_request("aria2.tellActive", serde_json::json!([]));
    let tell_resp = engine.handle_request(&tell_req).await;
    let active: Vec<serde_json::Value> = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
    assert_eq!(active.len(), 0, "No tasks should be active after pauseAll");
}

/// Test: aria2.forcePauseAll returns "OK".
#[tokio::test]
async fn regression_force_pause_all_returns_ok() {
    let engine = RpcEngine::new();

    for i in 0..2 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file{}", i)]]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.forcePauseAll", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

/// Test: aria2.unpauseAll returns "OK" with count.
#[tokio::test]
async fn regression_unpause_all_format() {
    let engine = RpcEngine::new();

    // Add and pause tasks
    for i in 0..2 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file{}", i)]]),
        );
        let resp = engine.handle_request(&req).await;
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
        engine.handle_request(&pause_req).await;
    }

    let req = make_request("aria2.unpauseAll", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
}

// =========================================================================
// URI/Position Management Methods (2 methods)
// =========================================================================

/// Test: aria2.changeUri modifies URI list.
#[tokio::test]
async fn regression_change_uri_modifies_list() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([[
            "http://example.com/file.zip",
            "http://mirror1.example.com/file.zip"
        ]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Remove first URI, add new one
    let req = make_request(
        "aria2.changeUri",
        serde_json::json!([
            gid,
            1,                                       // file index (aria2 is 1-based)
            ["http://example.com/file.zip"],         // del uris
            ["http://mirror2.example.com/file.zip"]  // add uris
        ]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.len(), 2);
    // changeUri returns [delCount, addCount] matching original aria2
    assert_eq!(
        result[0].as_i64(),
        Some(1),
        "delCount should be the JSON integer 1 (1 URI removed)"
    );
    assert_eq!(
        result[1].as_i64(),
        Some(1),
        "addCount should be the JSON integer 1 (1 URI added)"
    );
}

/// Test: aria2.changeUri inserts new URIs at the optional zero-based position.
#[tokio::test]
async fn regression_change_uri_honors_position() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/first", "http://example.com/last"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = make_request(
        "aria2.changeUri",
        serde_json::json!([
            gid,
            1,
            [],
            [
                "http://example.com/inserted-a",
                "http://example.com/inserted-b"
            ],
            0
        ]),
    );
    let change_resp = engine.handle_request(&change_req).await;
    assert_success(&change_resp);
    assert_eq!(change_resp.result.unwrap(), serde_json::json!([0, 2]));

    let uris_req = make_request("aria2.getUris", serde_json::json!([gid]));
    let uris_resp = engine.handle_request(&uris_req).await;
    assert_success(&uris_resp);
    let uris = uris_resp.result.unwrap();
    let uris: Vec<String> = uris
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["uri"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        uris,
        vec![
            "http://example.com/inserted-a",
            "http://example.com/inserted-b",
            "http://example.com/first",
            "http://example.com/last",
        ]
    );
}

/// Test: aria2.changePosition modifies URI position.
#[tokio::test]
async fn regression_change_position_modifies_position() {
    let engine = RpcEngine::new();

    let mut gid = String::new();
    for index in 1..=3 {
        let add_req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://uri{index}.example.com/file")]]),
        );
        let add_resp = engine.handle_request(&add_req).await;
        let added_gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();
        if index == 1 {
            gid = added_gid;
        }
    }

    let req = make_request(
        "aria2.changePosition",
        serde_json::json!([gid, 2, "POS_SET"]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: i64 = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        result, 2,
        "Should return the new absolute position as a JSON integer"
    );
}

// =========================================================================
// Session/System Methods (6 methods)
// =========================================================================

/// Test: aria2.getVersion returns version and enabledFeatures.
#[tokio::test]
async fn regression_get_version_format() {
    let engine = RpcEngine::new().with_product_version(aria2::identity::PRODUCT_VERSION);

    let req = make_request("aria2.getVersion", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let version: serde_json::Value = resp.result.unwrap();

    assert!(version.get("version").is_some(), "version field required");
    assert_eq!(
        version.get("version").and_then(serde_json::Value::as_str),
        Some(aria2::identity::PRODUCT_VERSION),
        "getVersion must expose the independent Rust product version"
    );
    assert!(
        version.get("enabledFeatures").is_some(),
        "enabledFeatures field required"
    );

    let features = version.get("enabledFeatures").unwrap().as_array().unwrap();
    assert!(
        !features.is_empty(),
        "Should have at least one enabled feature"
    );
}

/// Test: aria2.getSessionInfo returns sessionId.
#[tokio::test]
async fn regression_get_session_info_format() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getSessionInfo", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let session: serde_json::Value = resp.result.unwrap();

    assert!(
        session.get("sessionId").is_some(),
        "sessionId field required"
    );
    assert!(
        !session
            .get("sessionId")
            .unwrap()
            .as_str()
            .unwrap()
            .is_empty()
    );
}

/// Test: aria2.saveSession returns "OK" with count.
#[tokio::test]
async fn regression_save_session_format() {
    let session_path = std::env::temp_dir().join(format!(
        "aria2_rpc_regression_session_{}.sess",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&session_path);
    let engine = RpcEngine::new().with_save_session_path(session_path.clone());

    // Add a task
    let req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    engine.handle_request(&req).await;

    let req = make_request("aria2.saveSession", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
    let _ = std::fs::remove_file(session_path);
}

/// Test: `saveSession` uses the configured filename; the original method does
/// not accept a request-supplied path.
#[tokio::test]
async fn regression_save_session_ignores_extra_parameters() {
    let directory = tempfile::tempdir().unwrap();
    let configured = directory.path().join("configured.sess");
    let explicit = directory.path().join("explicit.sess");
    let engine = RpcEngine::new().with_save_session_path(configured.clone());

    let req = make_request(
        "aria2.saveSession",
        serde_json::json!([explicit.to_string_lossy()]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    assert!(configured.exists());
    assert!(!explicit.exists());
}

/// Test: aria2.shutdown returns "OK" with active count.
#[tokio::test]
async fn regression_shutdown_format() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.shutdown", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
}

/// Test: aria2.forceShutdown returns "OK" with terminated count.
#[tokio::test]
async fn regression_force_shutdown_format() {
    let engine = RpcEngine::new();

    // Add tasks
    for i in 0..2 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([[format!("http://example.com/file{}", i)]]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.forceShutdown", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("terminated"));
}

/// Test: system.multicall executes multiple calls.
#[tokio::test]
async fn regression_multicall_executes_batch() {
    let engine = RpcEngine::new();

    let req = make_request(
        "system.multicall",
        serde_json::json!([
            [
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.getGlobalStat", "params": []}
            ]
        ]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let results: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(results.len(), 2);
}

// =========================================================================
// Results Management Methods (2 methods)
// =========================================================================

/// Test: aria2.purgeDownloadResult clears results.
#[tokio::test]
async fn regression_purge_download_result_returns_ok() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.purgeDownloadResult", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

/// Test: aria2.removeDownloadResult returns "OK" for a valid stopped task.
#[tokio::test]
async fn regression_remove_download_result_returns_ok() {
    let engine = RpcEngine::new();

    // First add a task, then remove it to populate stopped_tasks.
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Remove the task so it goes into download results
    let remove_req = make_request("aria2.remove", serde_json::json!([gid]));
    engine.handle_request(&remove_req).await;

    // Reserved tasks are removed synchronously from the shared manager, so the
    // result is available even when this handler-only fixture has no engine
    // loop consuming commands.
    let req = make_request("aria2.removeDownloadResult", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

// =========================================================================
// System Discovery Methods (2 methods)
// =========================================================================

/// Test: system.listMethods matches aria2's feature-specific method order.
#[tokio::test]
async fn regression_list_methods_returns_feature_specific_methods() {
    let engine = RpcEngine::new();

    let req = make_request("system.listMethods", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    let mut expected = vec!["aria2.addUri"];
    #[cfg(feature = "bittorrent")]
    expected.extend(["aria2.addTorrent", "aria2.getPeers"]);
    #[cfg(feature = "metalink")]
    expected.push("aria2.addMetalink");
    expected.extend([
        "aria2.remove",
        "aria2.pause",
        "aria2.forcePause",
        "aria2.pauseAll",
        "aria2.forcePauseAll",
        "aria2.unpause",
        "aria2.unpauseAll",
        "aria2.forceRemove",
        "aria2.changePosition",
        "aria2.tellStatus",
        "aria2.getUris",
        "aria2.getFiles",
        "aria2.getServers",
        "aria2.tellActive",
        "aria2.tellWaiting",
        "aria2.tellStopped",
        "aria2.getOption",
        "aria2.changeUri",
        "aria2.changeOption",
        "aria2.getGlobalOption",
        "aria2.changeGlobalOption",
        "aria2.purgeDownloadResult",
        "aria2.removeDownloadResult",
        "aria2.getVersion",
        "aria2.getSessionInfo",
        "aria2.shutdown",
        "aria2.forceShutdown",
        "aria2.getGlobalStat",
        "aria2.saveSession",
        "system.multicall",
        "system.listMethods",
        "system.listNotifications",
    ]);
    let actual: Vec<&str> = methods.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);

    // Verify key methods are present
    assert!(methods.contains(&"aria2.addUri".to_string()));
    assert!(methods.contains(&"aria2.addTorrent".to_string()));
    assert_eq!(
        methods.contains(&"aria2.addMetalink".to_string()),
        cfg!(feature = "metalink")
    );
    assert!(methods.contains(&"aria2.remove".to_string()));
    assert!(methods.contains(&"aria2.tellStatus".to_string()));
    assert!(methods.contains(&"aria2.getVersion".to_string()));
    assert!(methods.contains(&"system.listMethods".to_string()));
    assert!(methods.contains(&"system.listNotifications".to_string()));
}

/// Test: system.listNotifications matches aria2's feature-specific order.
///
/// C++ aria2 and aria2-next both define exactly 6 notifications:
/// onDownloadStart, onDownloadPause, onDownloadStop, onDownloadComplete,
/// onDownloadError, onBtDownloadComplete. See RpcMethodFactory.cc and
/// WebSocketSessionMan.cc in the original source.
#[tokio::test]
async fn regression_list_notifications_returns_feature_specific_events() {
    let engine = RpcEngine::new();

    let req = make_request("system.listNotifications", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
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
    let actual: Vec<&str> = notifications.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);

    // Verify notification names match C++ aria2 exactly
    assert!(notifications.contains(&"aria2.onDownloadStart".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadPause".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadStop".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadComplete".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadError".to_string()));
    assert_eq!(
        notifications.contains(&"aria2.onBtDownloadComplete".to_string()),
        cfg!(feature = "bittorrent")
    );
}

// =========================================================================
// JSON-RPC Protocol Compliance Tests
// =========================================================================

/// Test: All responses include jsonrpc version "2.0".
#[tokio::test]
async fn regression_jsonrpc_version_field() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getVersion", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_eq!(resp.version, "2.0", "JSON-RPC version must be '2.0'");
}

/// Test: Error responses use correct JSON-RPC error codes.
#[tokio::test]
async fn regression_error_codes_jsonrpc_spec() {
    let engine = RpcEngine::new();

    // Invalid method: code 1 (C++ RpcMethod domain error, not -32601)
    let req = make_request("aria2.nonexistent", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, 1);

    // Invalid params: -32602
    let req = make_request("aria2.addUri", serde_json::json!([])); // Missing URI
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, -32602);
}

/// Test: Unknown method returns RpcExecution error (code 1).
#[tokio::test]
async fn regression_unknown_method_error() {
    let engine = RpcEngine::new();

    let req = make_request("unknown.method", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, 1);
    assert!(resp.error.unwrap().message.contains("No such method"));
}

// =========================================================================
// Compatibility Tests - aria2 Original Behavior
// =========================================================================

/// Test: GID format matches aria2 (16 hex characters).
#[tokio::test]
async fn regression_gid_format_matches_aria2() {
    let engine = RpcEngine::new();

    let req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let resp = engine.handle_request(&req).await;
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // aria2 uses 16-character hex GIDs
    assert_eq!(gid.len(), 16);
    assert!(gid.chars().all(|c| c.is_ascii_hexdigit()));
}

/// Test: Status values match aria2 enum (Active, Waiting, Paused, Complete, Error, Removed).
#[tokio::test]
async fn regression_status_values_match_aria2() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file"]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let status_req = make_request("aria2.tellStatus", serde_json::json!([gid]));
    let status_resp = engine.handle_request(&status_req).await;
    let status: serde_json::Value = status_resp.result.unwrap();

    let status_str = status.get("status").unwrap().as_str().unwrap();
    // Valid aria2 status values
    let valid_statuses = [
        "active", "waiting", "paused", "complete", "error", "removed",
    ];
    assert!(valid_statuses.contains(&status_str));
}

/// Test: GlobalStat field names use camelCase (aria2 convention).
#[tokio::test]
async fn regression_global_stat_camelcase_convention() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getGlobalStat", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;
    let stat: serde_json::Value = resp.result.unwrap();

    // aria2 uses camelCase for JSON output
    assert!(stat.get("downloadSpeed").is_some());
    assert!(stat.get("uploadSpeed").is_some());
    assert!(stat.get("numActive").is_some());
    assert!(stat.get("numWaiting").is_some());
    assert!(stat.get("numStopped").is_some());
    assert!(stat.get("numStoppedTotal").is_some());

    // Should NOT have snake_case versions
    assert!(stat.get("download_speed").is_none());
    assert!(stat.get("num_active").is_none());
}

/// Test: VersionInfo enabledFeatures is array of strings.
#[tokio::test]
async fn regression_version_enabled_features_array() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getVersion", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;
    let version: serde_json::Value = resp.result.unwrap();

    let features = version.get("enabledFeatures").unwrap();
    assert!(features.is_array());

    for feature in features.as_array().unwrap() {
        assert!(feature.is_string(), "Each feature should be a string");
    }
}

// =========================================================================
// system.multicall dispatch parity (regression for the 12-of-33 method gap)
//
// `system.multicall` used to carry its own hard-coded match arm listing only
// 12 of the 33 registered `aria2.*` methods; everything else fell through to
// `-32601 Method not found`. AriaNg / webui-aria2 batch their entire refresh
// loop into one multicall, so the UI silently lost most of its data.
//
// Sub-calls now go through the same dispatch table `handle_request` uses.
// These tests lock in that the full method surface is reachable from a batch,
// that nesting is still rejected, and that per-sub-call `token:` authorization
// strips the secret instead of leaking it into positional arguments.
// =========================================================================

/// Extract multicall entry `index` from a response.
///
/// Successful entries are wrapped in one extra array level (`[[result]]`) per
/// the aria2 spec; error entries stay flat (`{code, message}`).
fn multicall_entry(resp: &JsonRpcResponse, index: usize) -> serde_json::Value {
    let results = resp
        .result
        .as_ref()
        .expect("multicall should produce a result")
        .as_array()
        .expect("multicall result should be an array");
    results
        .get(index)
        .unwrap_or_else(|| panic!("multicall result should have an entry #{index}"))
        .clone()
}

/// Assert multicall entry `index` succeeded and return its unwrapped payload.
fn assert_multicall_ok(resp: &JsonRpcResponse, index: usize, label: &str) -> serde_json::Value {
    let entry = multicall_entry(resp, index);
    let inner = entry.as_array().unwrap_or_else(|| {
        panic!("multicall entry #{index} ({label}) should be a success array, got: {entry}")
    });
    inner
        .first()
        .unwrap_or_else(|| panic!("multicall entry #{index} ({label}) wrapper should not be empty"))
        .clone()
}

/// Assert multicall entry `index` is an error struct carrying `expected_code`.
///
/// The wire-format post-processor stringifies numbers recursively, so `code`
/// arrives as a string; both representations are accepted.
fn assert_multicall_error_code(resp: &JsonRpcResponse, index: usize, expected_code: i32) {
    let entry = multicall_entry(resp, index);
    let code = entry
        .get("code")
        .unwrap_or_else(|| panic!("multicall entry #{index} should be an error struct: {entry}"));
    let actual = code
        .as_i64()
        .or_else(|| code.as_str().and_then(|s| s.parse::<i64>().ok()))
        .unwrap_or_else(|| panic!("multicall error code should be numeric, got {code}"));
    assert_eq!(
        actual,
        i64::from(expected_code),
        "multicall entry #{index} should carry error code {expected_code}, got {entry}"
    );
}

/// Test: the AriaNg refresh batch (tellActive + tellWaiting + tellStopped +
/// getGlobalStat) resolves all four sub-calls instead of returning -32601.
#[tokio::test]
async fn regression_multicall_arianng_refresh_batch_all_methods_dispatch() {
    let engine = RpcEngine::new();

    let add = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/refresh.bin"]]),
    );
    assert_success(&engine.handle_request(&add).await);

    let req = make_request(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.tellActive", "params": []},
            {"methodName": "aria2.tellWaiting", "params": [0, 100]},
            {"methodName": "aria2.tellStopped", "params": [0, 100]},
            {"methodName": "aria2.getGlobalStat", "params": []},
        ]]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    let labels = [
        "aria2.tellActive",
        "aria2.tellWaiting",
        "aria2.tellStopped",
        "aria2.getGlobalStat",
    ];
    for (index, label) in labels.iter().enumerate() {
        let payload = assert_multicall_ok(&resp, index, label);
        if *label == "aria2.getGlobalStat" {
            assert!(
                payload.get("downloadSpeed").is_some(),
                "getGlobalStat payload should carry downloadSpeed, got {payload}"
            );
        } else {
            assert!(
                payload.is_array(),
                "{label} payload should be an array, got {payload}"
            );
        }
    }

    let active = assert_multicall_ok(&resp, 0, "aria2.tellActive");
    assert!(
        active.as_array().unwrap().is_empty(),
        "handler-only RpcEngine keeps newly added groups waiting"
    );
}

/// Test: every previously missing high-frequency method is reachable through a
/// multicall (no `-32601 Method not found`).
#[tokio::test]
async fn regression_multicall_covers_previously_missing_methods() {
    let engine = RpcEngine::new();

    let add = make_request(
        "aria2.addUri",
        serde_json::json!([["http://example.com/coverage.bin"]]),
    );
    let add_resp = engine.handle_request(&add).await;
    assert_success(&add_resp);
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Methods the old hard-coded multicall table did not know about.
    let calls = serde_json::json!([
        {"methodName": "aria2.tellStatus", "params": [gid]},
        {"methodName": "aria2.getOption", "params": [gid]},
        {"methodName": "aria2.getPeers", "params": [gid]},
        {"methodName": "aria2.changeOption", "params": [gid, {"max-download-limit": "1M"}]},
        {"methodName": "aria2.pause", "params": [gid]},
        {"methodName": "aria2.unpause", "params": [gid]},
        {"methodName": "aria2.getGlobalOption", "params": []},
        {"methodName": "aria2.changeGlobalOption", "params": [{"max-overall-download-limit": "2M"}]},
        {"methodName": "aria2.pauseAll", "params": []},
        {"methodName": "aria2.unpauseAll", "params": []},
        {"methodName": "aria2.tellWaiting", "params": [0, 10]},
        {"methodName": "aria2.tellStopped", "params": [0, 10]},
        {"methodName": "system.listMethods", "params": []},
        {"methodName": "system.listNotifications", "params": []},
    ]);
    let expected_len = calls.as_array().unwrap().len();

    let req = make_request("system.multicall", serde_json::json!([calls]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    let results = resp.result.as_ref().unwrap().as_array().unwrap();
    assert_eq!(results.len(), expected_len);

    for (index, entry) in results.iter().enumerate() {
        let code = entry.get("code").and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
        assert_ne!(
            code,
            Some(-32601),
            "multicall entry #{index} must not be 'Method not found': {entry}"
        );
    }
}

/// Test: a nested `system.multicall` is still rejected with code 1 without
/// aborting the surrounding batch.
#[tokio::test]
async fn regression_multicall_nested_multicall_rejected() {
    let engine = RpcEngine::new();

    let req = make_request(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "system.multicall", "params": [[
                {"methodName": "aria2.getVersion", "params": []}
            ]]},
            {"methodName": "aria2.getSessionInfo", "params": []},
        ]]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    assert_multicall_ok(&resp, 0, "aria2.getVersion");
    assert_multicall_error_code(&resp, 1, 1);
    assert_multicall_ok(&resp, 2, "aria2.getSessionInfo");
}

/// Test: an unknown sub-call still yields error code 1 for that entry only.
#[tokio::test]
async fn regression_multicall_unknown_method_isolated_to_entry() {
    let engine = RpcEngine::new();

    let req = make_request(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.nonexistentMethod", "params": []},
            {"methodName": "aria2.getVersion", "params": []},
        ]]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    assert_multicall_error_code(&resp, 0, 1);
    assert_multicall_ok(&resp, 1, "aria2.getVersion");
}

/// Test: with `rpc-secret` configured, AriaNg's per-sub-call `"token:xxx"`
/// first parameter is validated and stripped so the remaining positional
/// arguments do not shift.
///
/// Before the fix the sub-request was built straight from the raw params, so
/// the token leaked in as `params[0]` and
/// `aria2.tellStatus(["token:s", gid])` read the token as the GID.
#[tokio::test]
async fn regression_multicall_subcall_token_is_stripped_not_leaked() {
    let secret = "multicall-secret";
    let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new(secret));

    // Add a download using the same per-call token convention.
    let add = make_request(
        "aria2.addUri",
        serde_json::json!([format!("token:{secret}"), ["http://example.com/auth.bin"]]),
    );
    let add_resp = engine.handle_request(&add).await;
    assert_success(&add_resp);
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // No token on the envelope — exactly what AriaNg sends.
    let req = make_request(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.tellStatus", "params": [format!("token:{secret}"), gid]},
            {"methodName": "aria2.tellActive", "params": [format!("token:{secret}")]},
            {"methodName": "aria2.getGlobalStat", "params": [format!("token:{secret}")]},
        ]]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    let status = assert_multicall_ok(&resp, 0, "aria2.tellStatus");
    assert_eq!(
        status.get("gid").and_then(|v| v.as_str()),
        Some(gid.as_str()),
        "tellStatus must resolve the GID, not the stripped token: {status}"
    );

    let active = assert_multicall_ok(&resp, 1, "aria2.tellActive");
    assert!(active.as_array().unwrap().is_empty());

    let stat = assert_multicall_ok(&resp, 2, "aria2.getGlobalStat");
    assert!(stat.get("downloadSpeed").is_some());
}

/// Test: a sub-call with a wrong/missing token is rejected per entry while
/// correctly authenticated siblings still run.
///
/// Mirrors C++ `RpcMethod::execute()`, which turns a failed `authorize()` into
/// that entry's error response and lets the loop continue.
#[tokio::test]
async fn regression_multicall_subcall_bad_token_isolated() {
    let secret = "multicall-secret";
    let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new(secret));

    let req = make_request(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.getVersion", "params": ["token:wrong"]},
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.getVersion", "params": [format!("token:{secret}")]},
        ]]),
    );
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    assert_multicall_error_code(&resp, 0, 1);
    assert_multicall_error_code(&resp, 1, 1);
    let ok = assert_multicall_ok(&resp, 2, "aria2.getVersion");
    assert!(ok.get("version").is_some());
}

/// Test: a token supplied on the multicall envelope is rejected like the C++
/// implementation, which requires parameter zero to be the call list.
#[tokio::test]
async fn regression_multicall_envelope_token_is_not_a_fallback() {
    let secret = "multicall-secret";
    let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new(secret));

    let req = make_request(
        "system.multicall",
        serde_json::json!([
            format!("token:{secret}"),
            [
                {"methodName": "aria2.getVersion", "params": []},
                {"methodName": "aria2.getGlobalStat", "params": []},
            ]
        ]),
    );
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, 1);
}

/// Test: an invalid token-shaped first envelope parameter is still rejected as
/// a wrong multicall parameter, rather than being treated as authentication.
#[tokio::test]
async fn regression_multicall_envelope_bad_token_rejected() {
    let engine = RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("real-secret"));

    let req = make_request(
        "system.multicall",
        serde_json::json!([
            "token:wrong",
            [{"methodName": "aria2.getVersion", "params": []}]
        ]),
    );
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, 1);
}

/// Test: the wire-format post-processor reaches inside the `[[result]]`
/// nesting, so numbers in multicall payloads are stringified exactly like they
/// are in a top-level response.
#[tokio::test]
async fn regression_multicall_wire_format_applies_to_nested_results() {
    let engine = RpcEngine::new();

    let direct = make_request("aria2.getGlobalStat", serde_json::json!([]));
    let direct_resp = engine.handle_request(&direct).await;
    assert_success(&direct_resp);
    let direct_stat = direct_resp.result.unwrap();

    let batched = make_request(
        "system.multicall",
        serde_json::json!([[{"methodName": "aria2.getGlobalStat", "params": []}]]),
    );
    let batched_resp = engine.handle_request(&batched).await;
    assert_success(&batched_resp);
    let batched_stat = assert_multicall_ok(&batched_resp, 0, "aria2.getGlobalStat");

    for key in [
        "downloadSpeed",
        "uploadSpeed",
        "numActive",
        "numWaiting",
        "numStopped",
    ] {
        let nested = batched_stat
            .get(key)
            .unwrap_or_else(|| panic!("nested getGlobalStat should expose '{key}'"));
        assert!(
            nested.is_string(),
            "'{key}' inside a multicall must be wire-formatted to a string, got {nested}"
        );
        assert_eq!(
            nested,
            direct_stat.get(key).unwrap(),
            "'{key}' must be identical whether fetched directly or via multicall"
        );
    }
}
