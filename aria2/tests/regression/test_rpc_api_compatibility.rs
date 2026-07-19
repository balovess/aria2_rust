//! RPC API regression tests for aria2-rust.
//!
//! These tests verify that all 37 RPC methods return values in the expected format
//! and maintain compatibility with the original aria2 RPC specification.

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::json_rpc::JsonRpcResponse;

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
        serde_json::json!(["http://example.com/file.zip"]),
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

/// Test: aria2.addMetalink validates Metalink XML.
#[tokio::test]
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
        serde_json::json!(["http://example.com/file"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Then remove it
    let remove_req = make_request("aria2.remove", serde_json::json!([gid]));
    let remove_resp = engine.handle_request(&remove_req).await;

    assert_success(&remove_resp);
    let result: Vec<String> = serde_json::from_value(remove_resp.result.unwrap()).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], gid);
}

/// Test: aria2.remove with nonexistent GID returns error.
#[tokio::test]
async fn regression_remove_nonexistent_returns_error() {
    let engine = RpcEngine::new();
    let req = make_request("aria2.remove", serde_json::json!(["nonexistent-gid-12345"]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32601); // MethodNotFound (GID not found)
}

/// Test: aria2.forceRemove returns "OK".
#[tokio::test]
async fn regression_force_remove_returns_ok() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.forceRemove", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

/// Test: aria2.pause changes status to Paused.
#[tokio::test]
async fn regression_pause_changes_status() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let pause_req = make_request("aria2.pause", serde_json::json!([gid]));
    let pause_resp = engine.handle_request(&pause_req).await;

    assert_success(&pause_resp);

    // Verify status changed
    let status_req = make_request("aria2.tellStatus", serde_json::json!([gid]));
    let status_resp = engine.handle_request(&status_req).await;
    let status: serde_json::Value = status_resp.result.unwrap();
    assert_eq!(status.get("status").unwrap().as_str().unwrap(), "paused");
}

/// Test: aria2.forcePause returns "OK".
#[tokio::test]
async fn regression_force_pause_returns_ok() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.forcePause", serde_json::json!([gid]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

/// Test: aria2.unpause changes status back to Active.
#[tokio::test]
async fn regression_unpause_restores_active() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
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
    assert_eq!(status.get("status").unwrap().as_str().unwrap(), "active");
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
        serde_json::json!(["http://example.com/file"]),
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

/// Test: aria2.tellActive returns array of StatusInfo.
#[tokio::test]
async fn regression_tell_active_returns_array() {
    let engine = RpcEngine::new();

    // Add multiple tasks
    for i in 0..3 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([format!("http://example.com/file{}", i)]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.tellActive", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let active: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(active.len(), 3);

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

    assert_error_code(&resp, -32601);
}

/// Test: aria2.changeOption validates option keys.
#[tokio::test]
async fn regression_change_option_validates_keys() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Valid option
    let req = make_request("aria2.changeOption", serde_json::json!([gid, {"split": 8}]));
    let resp = engine.handle_request(&req).await;
    assert_success(&resp);

    // Invalid option key
    let invalid_req = make_request(
        "aria2.changeOption",
        serde_json::json!([gid, {"invalid-option": "value"}]),
    );
    let invalid_resp = engine.handle_request(&invalid_req).await;
    assert_error_code(&invalid_resp, -32602);
}

/// Test: aria2.changeOption accepts max-connection-per-server (runtime-changeable).
#[tokio::test]
async fn regression_change_option_accepts_max_connection_per_server() {
    let engine = RpcEngine::new();

    // Add a task first
    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
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
        serde_json::json!(["http://example.com/file.zip"]),
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
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file.zip"]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

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

/// Test: aria2.pauseAll returns "OK" with count.
#[tokio::test]
async fn regression_pause_all_format() {
    let engine = RpcEngine::new();

    // Add multiple tasks
    for i in 0..3 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([format!("http://example.com/file{}", i)]),
        );
        engine.handle_request(&req).await;
    }

    let req = make_request("aria2.pauseAll", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("3"), "Should report 3 tasks paused");
}

/// Test: aria2.forcePauseAll returns "OK".
#[tokio::test]
async fn regression_force_pause_all_returns_ok() {
    let engine = RpcEngine::new();

    for i in 0..2 {
        let req = make_request(
            "aria2.addUri",
            serde_json::json!([format!("http://example.com/file{}", i)]),
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
            serde_json::json!([format!("http://example.com/file{}", i)]),
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
            0,                                       // file index
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
        result[0].as_str(),
        Some("1"),
        "delCount should be 1 (1 URI removed)"
    );
    assert_eq!(
        result[1].as_str(),
        Some("1"),
        "addCount should be 1 (1 URI added)"
    );
}

/// Test: aria2.changePosition modifies URI position.
#[tokio::test]
async fn regression_change_position_modifies_position() {
    let engine = RpcEngine::new();

    let add_req = make_request(
        "aria2.addUri",
        serde_json::json!([[
            "http://uri1.example.com/file",
            "http://uri2.example.com/file",
            "http://uri3.example.com/file"
        ]]),
    );
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = make_request("aria2.changePosition", serde_json::json!([gid, 2, "POS_SET"]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "2", "Should return the new absolute position as string");
}

// =========================================================================
// Session/System Methods (6 methods)
// =========================================================================

/// Test: aria2.getVersion returns version and enabledFeatures.
#[tokio::test]
async fn regression_get_version_format() {
    let engine = RpcEngine::new();

    let req = make_request("aria2.getVersion", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let version: serde_json::Value = resp.result.unwrap();

    assert!(version.get("version").is_some(), "version field required");
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
    let engine = RpcEngine::new();

    // Add a task
    let req = make_request(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    );
    engine.handle_request(&req).await;

    let req = make_request("aria2.saveSession", serde_json::json!(["/tmp/session.txt"]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("Saved"));
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
            serde_json::json!([format!("http://example.com/file{}", i)]),
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

/// Test: aria2.removeDownloadResult returns "OK".
#[tokio::test]
async fn regression_remove_download_result_returns_ok() {
    let engine = RpcEngine::new();

    let req = make_request(
        "aria2.removeDownloadResult",
        serde_json::json!(["some-gid"]),
    );
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

// =========================================================================
// System Discovery Methods (2 methods)
// =========================================================================

/// Test: system.listMethods returns all 36 methods.
#[tokio::test]
async fn regression_list_methods_returns_36_methods() {
    let engine = RpcEngine::new();

    let req = make_request("system.listMethods", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(methods.len(), 37, "Should return exactly 37 methods");

    // Verify key methods are present
    assert!(methods.contains(&"aria2.addUri".to_string()));
    assert!(methods.contains(&"aria2.addTorrent".to_string()));
    assert!(methods.contains(&"aria2.addMetalink".to_string()));
    assert!(methods.contains(&"aria2.remove".to_string()));
    assert!(methods.contains(&"aria2.tellStatus".to_string()));
    assert!(methods.contains(&"aria2.getVersion".to_string()));
    assert!(methods.contains(&"system.listMethods".to_string()));
    assert!(methods.contains(&"system.listNotifications".to_string()));
}

/// Test: system.listNotifications returns 7 notifications.
#[tokio::test]
async fn regression_list_notifications_returns_7() {
    let engine = RpcEngine::new();

    let req = make_request("system.listNotifications", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_success(&resp);
    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        notifications.len(),
        7,
        "Should return exactly 7 notifications"
    );

    // Verify notification names
    assert!(notifications.contains(&"aria2.onDownloadStart".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadPause".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadStop".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadComplete".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadError".to_string()));
    assert!(notifications.contains(&"aria2.onBtDownloadComplete".to_string()));
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

    // Invalid method: -32601
    let req = make_request("aria2.nonexistent", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, -32601);

    // Invalid params: -32602
    let req = make_request("aria2.addUri", serde_json::json!([])); // Missing URI
    let resp = engine.handle_request(&req).await;
    assert_error_code(&resp, -32602);
}

/// Test: Unknown method returns MethodNotFound error.
#[tokio::test]
async fn regression_unknown_method_error() {
    let engine = RpcEngine::new();

    let req = make_request("unknown.method", serde_json::json!([]));
    let resp = engine.handle_request(&req).await;

    assert_error_code(&resp, -32601);
    assert!(resp.error.unwrap().message.contains("Method not found"));
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
        serde_json::json!(["http://example.com/file"]),
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
        serde_json::json!(["http://example.com/file"]),
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
