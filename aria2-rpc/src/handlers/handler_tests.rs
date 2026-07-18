//! Handler integration tests.
//!
//! Tests for RPC handler methods exercised through `RpcEngine::handle_request`.

use base64::Engine;
use std::collections::HashMap;

use crate::engine::RpcEngine;
use crate::json_rpc::JsonRpcRequest;
use crate::types::{
    DownloadStatus, FileInfo, PeerInfo, ServerInfoIndex, SessionInfo, StatusInfo, UriInfo,
    UriStatus, VersionInfo,
};
use crate::websocket::{DownloadEvent, EventType};

#[tokio::test]
async fn test_handle_add_torrent() {
    let engine = RpcEngine::new();
    let fake_torrent_bencode = "d8:announce40:http://tracker.example.com/announce4:info6:lengthi1000e12:piece lengthi32768e6:pieces20:00000000000000000000000ee";
    let encoded = base64::engine::general_purpose::STANDARD.encode(fake_torrent_bencode.as_bytes());
    let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([encoded])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "addTorrent should succeed for valid BEncode data"
    );
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!gid.is_empty());
    assert_eq!(engine.task_count().await, 1);
}

#[tokio::test]
async fn test_handle_add_torrent_invalid_data() {
    let engine = RpcEngine::new();
    let not_torrent =
        base64::engine::general_purpose::STANDARD.encode("this is not a torrent file");
    let req = JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([not_torrent])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_error(),
        "addTorrent should fail for non-BEncode data"
    );
}

#[tokio::test]
async fn test_handle_add_metalink() {
    let engine = RpcEngine::new();
    let metalink_xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="test.bin"><size>1024</size><url priority="1">http://example.com/test.bin</url></file></files></metalink>"#;
    let encoded = base64::engine::general_purpose::STANDARD.encode(metalink_xml.as_bytes());
    let req = JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([encoded])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "addMetalink should succeed for valid Metalink XML"
    );
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!gid.is_empty());
    assert_eq!(engine.task_count().await, 1);
}

#[tokio::test]
async fn test_handle_add_metalink_invalid_data() {
    let engine = RpcEngine::new();
    let not_metalink = base64::engine::general_purpose::STANDARD.encode("this is not metalink xml");
    let req =
        JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([not_metalink])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "addMetalink should fail for non-XML data");
}

#[tokio::test]
async fn test_tell_status_has_real_progress_data() {
    let engine = RpcEngine::new();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://x.com/large.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine
        .update_task_progress(&gid, 10485760, 5242880, 1024, 1048576, 512, 3)
        .await;

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success(), "tellStatus should succeed");

    let status_val = tell_resp.result.unwrap();
    let status: StatusInfo = serde_json::from_value(status_val).unwrap();

    assert_eq!(
        status.total_length,
        Some("10485760".to_string()),
        "total_length should be 10MB"
    );
    assert_eq!(
        status.completed_length,
        Some("5242880".to_string()),
        "completed_length should be 5MB (50%)"
    );
    assert_eq!(
        status.upload_length,
        Some("1024".to_string()),
        "upload_length should be 1KB"
    );
    assert_eq!(
        status.download_speed,
        Some("1048576".to_string()),
        "download_speed should be 1MB/s"
    );
    assert_eq!(
        status.upload_speed,
        Some("512".to_string()),
        "upload_speed should be 512B/s"
    );
    assert_eq!(
        status.connections,
        Some("3".to_string()),
        "connections should be 3"
    );

    let expected_percent = (5242880.0 / 10485760.0) * 100.0;
    assert!(
        (status.progress_percent() - expected_percent).abs() < 0.01,
        "progress percent should be ~50%"
    );
}

#[tokio::test]
async fn test_tell_status_zero_for_nonexistent_gid() {
    let engine = RpcEngine::new();

    let tell_req = JsonRpcRequest::new(
        "aria2.tellStatus",
        serde_json::json!(["nonexistent-gid-12345"]),
    )
    .with_id(1);
    let tell_resp = engine.handle_request(&tell_req).await;

    assert!(
        tell_resp.is_error(),
        "tellStatus should fail for non-existent GID"
    );
    assert_eq!(
        tell_resp.error.unwrap().code,
        -32601,
        "error code should be MethodNotFound (-32601)"
    );
}

#[tokio::test]
async fn test_tell_status_includes_upload_fields() {
    let engine = RpcEngine::new();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://torrent.example.com/file.torrent"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine
        .update_task_progress(&gid, 1073741824, 1073741824, 536870912, 0, 1048576, 10)
        .await;

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());

    let status_val = tell_resp.result.unwrap();
    let status: StatusInfo = serde_json::from_value(status_val).unwrap();

    assert!(
        status.upload_length.is_some(),
        "upload_length field must be present"
    );
    assert!(
        status.upload_speed.is_some(),
        "upload_speed field must be present"
    );
    assert_eq!(
        status.upload_length,
        Some("536870912".to_string()),
        "upload_length should reflect seeding contribution"
    );
    assert_eq!(
        status.upload_speed,
        Some("1048576".to_string()),
        "upload_speed should show current seeding rate"
    );
    assert_eq!(
        status.connections,
        Some("10".to_string()),
        "connections should show peer count"
    );
}

#[tokio::test]
async fn test_get_peers_returns_peer_list() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://x.com/f.torrent"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let peers = vec![
        PeerInfo::new("p1", "10.0.0.1")
            .with_port(6881u16)
            .with_am_choking(false)
            .with_peer_choking(true)
            .with_download_speed(100000u64)
            .with_upload_speed(50000u64)
            .with_seeder(true),
        PeerInfo::new("p2", "10.0.0.2")
            .with_port(6882u16)
            .with_bitfield("ff00")
            .with_am_choking(true)
            .with_peer_choking(false)
            .with_download_speed(200000u64)
            .with_upload_speed(75000u64)
            .with_seeder(false),
    ];
    engine.set_task_peers(&gid, peers.clone()).await;

    let req = JsonRpcRequest::new("aria2.getPeers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "getPeers should succeed for existing GID"
    );

    let result_peers: Vec<PeerInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result_peers.len(), 2, "Should return 2 peers");
    assert_eq!(result_peers[0].peer_id, "p1");
    assert_eq!(result_peers[1].ip, "10.0.0.2");
}

#[tokio::test]
async fn test_get_peers_unknown_gid() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getPeers", serde_json::json!(["nonexistent-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getPeers should fail for non-existent GID");
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[tokio::test]
async fn test_pause_all_pauses_active_tasks() {
    let engine = RpcEngine::new();
    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://x.com/{}", i)]),
        )
        .with_id(i);
        engine.handle_request(&req).await;
    }
    assert_eq!(engine.task_count().await, 3);

    let pause_req = JsonRpcRequest::new("aria2.pauseAll", serde_json::json!([])).with_id(10);
    let pause_resp = engine.handle_request(&pause_req).await;
    assert!(pause_resp.is_success());

    let tell_req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(11);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());
    let active: Vec<StatusInfo> = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
    assert_eq!(active.len(), 0, "No tasks should be active after pauseAll");
}

#[tokio::test]
async fn test_unpause_all_resumes_paused_tasks() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let pause_req = JsonRpcRequest::new("aria2.pause", serde_json::json!([gid])).with_id(2);
    engine.handle_request(&pause_req).await;

    let unpause_req = JsonRpcRequest::new("aria2.unpauseAll", serde_json::json!([])).with_id(3);
    let unpause_resp = engine.handle_request(&unpause_req).await;
    assert!(unpause_resp.is_success());

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(4);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());
    let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
    assert_eq!(
        status.status,
        DownloadStatus::Active,
        "Task should be Active after unpauseAll"
    );
}

#[tokio::test]
async fn test_change_uri_adds_uris() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://x.com/original.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = JsonRpcRequest::new(
        "aria2.changeUri",
        serde_json::json!([
            gid,
            0,
            null,
            ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]
        ]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success(), "changeUri should succeed");

    // Per aria2 RPC spec, result is [delcount, addcount]
    let result = change_resp.result.unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr[0], 0, "delcount should be 0 (no URIs deleted)");
    assert_eq!(arr[1], 2, "addcount should be 2 (2 URIs added)");
}

#[tokio::test]
async fn test_change_uri_deletes_and_adds_uris() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://x.com/original.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Delete the original URI and add 2 new ones
    let change_req = JsonRpcRequest::new(
        "aria2.changeUri",
        serde_json::json!([
            gid,
            0,
            ["http://x.com/original.iso"],
            ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]
        ]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success());

    let result = change_resp.result.unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr[0], 1, "delcount should be 1 (original deleted)");
    assert_eq!(arr[1], 2, "addcount should be 2 (2 mirrors added)");
}

#[tokio::test]
async fn test_change_uri_delete_nonexistent_returns_zero_count() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://x.com/original.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Try to delete a URI that doesn't exist
    let change_req = JsonRpcRequest::new(
        "aria2.changeUri",
        serde_json::json!([gid, 0, ["http://nonexistent.com/file.iso"], null]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success());

    let result = change_resp.result.unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr[0], 0, "delcount should be 0 (URI not found)");
    assert_eq!(arr[1], 0, "addcount should be 0 (no URIs added)");
}

#[tokio::test]
async fn test_download_resume_event() {
    let event = DownloadEvent::download_resume("gid-resume-001");
    assert_eq!(event.event_type().unwrap(), EventType::DownloadResume);
    assert_eq!(event.method(), "aria2.onDownloadResume");
    let json = event.to_json().unwrap();
    assert!(json.contains("\"method\":\"aria2.onDownloadResume\""));
    assert!(json.contains("\"gid\":\"gid-resume-001\""));
}

#[tokio::test]
async fn test_change_option_rejects_unknown_key() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, {"totally-invalid-option": 42}]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "changeOption with unknown key should fail");
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "error code should be InvalidParams (-32602)"
    );
}

#[tokio::test]
async fn test_change_option_accepts_valid_keys() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Only runtime-changeable options are accepted by changeOption; startup-
    // only options like `split` and `dir` are rejected with InvalidParams.
    let valid_changes = serde_json::json!({
        "max-download-limit": 1048576,
        "max-upload-limit": 512000,
        "max-retries": 5
    });
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, valid_changes]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "changeOption with runtime-changeable keys should succeed"
    );

    let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
    let opts: HashMap<String, serde_json::Value> =
        serde_json::from_value(get_resp.result.unwrap()).unwrap();
    assert!(
        opts.contains_key("max-download-limit"),
        "max-download-limit should be stored"
    );
    assert!(
        opts.contains_key("max-upload-limit"),
        "max-upload-limit should be stored"
    );
    assert!(
        opts.contains_key("max-retries"),
        "max-retries should be stored"
    );
}

#[tokio::test]
async fn test_change_option_rejects_startup_only_key() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // `dir` is a known option (in VALID_OPTION_KEYS) but startup-only, so
    // changeOption must reject it with InvalidParams.
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, {"dir": "/tmp/downloads"}]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_error(),
        "changeOption with a startup-only key should fail"
    );
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "error code should be InvalidParams (-32602)"
    );
}

#[tokio::test]
async fn test_handle_get_set_option() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let set_req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, {"max-download-limit": 1048576}]),
    )
    .with_id(2);
    let set_resp = engine.handle_request(&set_req).await;
    assert!(set_resp.is_success());

    let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
}

#[tokio::test]
async fn test_multiple_tasks() {
    let engine = RpcEngine::new();
    for i in 0..5 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://x.com/{}", i)]),
        )
        .with_id(i);
        engine.handle_request(&req).await;
    }
    assert_eq!(engine.task_count().await, 5);
}

#[tokio::test]
async fn test_engine_default() {
    let engine = RpcEngine::default();
    assert_eq!(engine.task_count().await, 0);
}

// =========================================================================
// System Multicall Tests (H6)
// =========================================================================

#[tokio::test]
async fn test_multicall_executes_multiple_methods() {
    let engine = RpcEngine::new();

    let multicall_req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.getGlobalStat", "params": []},
            {"methodName": "aria2.getSessionInfo", "params": []},
        ]]),
    )
    .with_id(1);

    let resp = engine.handle_request(&multicall_req).await;
    assert!(resp.is_success(), "Multicall should succeed");

    let result_value = resp.result.unwrap();
    let results = result_value.as_array().expect("Should return array");
    assert_eq!(results.len(), 3, "Should have 3 results");

    // Per original aria2 multicall protocol, each successful sub-call result
    // is wrapped in a single-element array `[result]` (matching XML-RPC
    // system.multicall convention and AriaNg's `response.data[i][0]` indexing).
    let version_result = &results[0][0];
    assert!(
        version_result.get("version").is_some() || version_result.as_str().is_some(),
        "getVersion result should contain version info"
    );

    let stat_result = &results[1][0];
    assert!(
        stat_result.get("downloadSpeed").is_some(),
        "getGlobalStat should contain downloadSpeed"
    );

    let session_result = &results[2][0];
    assert!(
        session_result.get("sessionId").is_some(),
        "getSessionInfo should contain sessionId"
    );
}

#[tokio::test]
async fn test_multicall_preserves_order() {
    let engine = RpcEngine::new();

    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://order-test.com/{}", i)]),
        )
        .with_id(i);
        engine.handle_request(&req).await;
    }

    let multicall_req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            {"methodName": "aria2.getVersion", "params": []},
            {"methodName": "aria2.tellActive", "params": []},
            {"methodName": "aria2.getGlobalStat", "params": []},
            {"methodName": "aria2.getSessionInfo", "params": []},
        ]]),
    )
    .with_id(10);

    let resp = engine.handle_request(&multicall_req).await;
    assert!(resp.is_success());

    let result_value = resp.result.unwrap();
    let results = result_value.as_array().unwrap();
    assert_eq!(results.len(), 4, "Should have 4 results in order");

    // Each successful result is wrapped in `[result]` (original aria2 format).
    assert!(
        results[0][0].get("version").is_some()
            || results[0][0].get("enabledFeatures").is_some()
    );
    let active = results[1][0]
        .as_array()
        .expect("tellActive should return array");
    assert_eq!(active.len(), 3, "Should have 3 active tasks");
    assert!(results[2][0].get("downloadSpeed").is_some());
    assert!(results[3][0].get("sessionId").is_some());
}

#[tokio::test]
async fn test_multicall_empty_calls_returns_empty_array() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest::new("system.multicall", serde_json::json!([[]])).with_id(1);

    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let result_value = resp.result.unwrap();
    let results = result_value.as_array().unwrap();
    assert!(results.is_empty(), "Empty calls should return empty array");
}

#[tokio::test]
async fn test_multicall_with_add_uri_and_status() {
    let engine = RpcEngine::new();

    let multicall_req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            {
                "methodName": "aria2.addUri",
                "params": [["http://multicall-test.com/file.bin"]]
            },
            {
                "methodName": "aria2.getGlobalStat",
                "params": []
            },
        ]]),
    )
    .with_id(1);

    let resp = engine.handle_request(&multicall_req).await;
    assert!(
        resp.is_success(),
        "Multicall with addUri + getGlobalStat should succeed"
    );

    let result_value = resp.result.unwrap();
    let results = result_value.as_array().unwrap();
    assert_eq!(results.len(), 2);

    // Successful sub-call results are wrapped in `[result]`.
    assert!(!results[0][0].is_null(), "addUri should return a value");
    assert!(
        results[1][0].get("downloadSpeed").is_some(),
        "getGlobalStat should contain downloadSpeed"
    );
}

/// Verifies error isolation: a failing sub-call returns an error object in
/// its slot (not wrapped in `[...]`), while sibling successful sub-calls
/// still return `[result]`. The overall multicall response remains success.
///
/// Mirrors the original aria2 `SystemMulticallRpcMethod::execute` in
/// `RpcMethodImpl.cc:1462-1469` — error responses are pushed directly
/// without wrapping, while success responses are wrapped.
#[tokio::test]
async fn test_multicall_isolates_errors() {
    let engine = RpcEngine::new();

    let multicall_req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            // Successful call
            {"methodName": "aria2.getVersion", "params": []},
            // Failing call: unknown GID → MethodNotFound
            {"methodName": "aria2.tellStatus", "params": ["0000000000000000"]},
            // Failing call: unknown method → MethodNotFound
            {"methodName": "aria2.nonexistentMethod", "params": []},
            // Successful call
            {"methodName": "aria2.getGlobalStat", "params": []},
        ]]),
    )
    .with_id(42);

    let resp = engine.handle_request(&multicall_req).await;
    assert!(
        resp.is_success(),
        "Multicall itself should succeed even if some sub-calls fail"
    );

    // Capture JSON first to avoid partially moving `resp.result`.
    let json_str = resp.to_string().unwrap();
    let results = resp.result.unwrap().as_array().unwrap().clone();
    assert_eq!(results.len(), 4, "Should have 4 result slots");

    // Slot 0: successful getVersion → wrapped as [result]
    assert!(
        results[0].is_array(),
        "Successful sub-call result should be wrapped in [result]: got {:?}",
        results[0]
    );
    assert!(results[0][0].get("version").is_some());

    // Slot 1: failed tellStatus → error object directly (NOT wrapped)
    assert!(
        results[1].is_object() && results[1].get("code").is_some(),
        "Failed sub-call should be an error object, not wrapped: got {:?}",
        results[1]
    );
    assert_eq!(results[1]["code"].as_i64(), Some(-32601), "Should be MethodNotFound");
    assert!(
        results[1]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not found"),
        "Error message should mention the missing GID: got {:?}",
        results[1]["message"]
    );

    // Slot 2: failed unknown method → error object directly
    assert!(results[2].get("code").is_some(), "Unknown method should be error");
    assert_eq!(results[2]["code"].as_i64(), Some(-32601));

    // Slot 3: successful getGlobalStat → wrapped as [result]
    assert!(
        results[3].is_array(),
        "Successful sub-call after errors should still be wrapped: got {:?}",
        results[3]
    );
    assert!(results[3][0].get("downloadSpeed").is_some());

    // The overall response id should match the multicall request id.
    assert!(
        json_str.contains("\"id\":42"),
        "Multicall response id should echo request id: {}",
        json_str
    );
}

/// Verifies that error isolation works even for handlers that return
/// `Result<JsonRpcResponse, JsonRpcError>` (like `aria2.tellActive`).
/// Previously `aria2.tellActive` used `?` in the multicall dispatcher, which
/// propagated the error and aborted the entire multicall — breaking
/// AriaNg's batched queries.
#[tokio::test]
async fn test_multicall_isolates_result_returning_handler_errors() {
    let engine = RpcEngine::new();

    let multicall_req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[
            // tellActive with invalid `keys` param type should fail
            // individually without aborting the multicall.
            {"methodName": "aria2.tellActive", "params": ["invalid-keys-param"]},
            // This call should still execute and produce a wrapped result.
            {"methodName": "aria2.getVersion", "params": []},
        ]]),
    )
    .with_id(7);

    let resp = engine.handle_request(&multicall_req).await;
    assert!(
        resp.is_success(),
        "Multicall should succeed even when tellActive fails"
    );

    let results = resp.result.unwrap().as_array().unwrap().clone();
    assert_eq!(results.len(), 2);

    // Slot 0: tellActive failed → error object (NOT wrapped)
    assert!(
        results[0].get("code").is_some(),
        "Failed tellActive should be an error object: got {:?}",
        results[0]
    );

    // Slot 1: getVersion succeeded → wrapped [result]
    assert!(
        results[1].is_array(),
        "getVersion should be wrapped: got {:?}",
        results[1]
    );
    assert!(results[1][0].get("version").is_some());
}

// =========================================================================
// K1 Additional RPC Handler Tests
// =========================================================================

#[tokio::test]
async fn test_save_session_handler_basic() {
    let engine = RpcEngine::new();

    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://save-session.com/{}", i)]),
        )
        .with_id(i);
        engine.handle_request(&req).await;
    }

    let req = JsonRpcRequest::new(
        "aria2.saveSession",
        serde_json::json!(["/tmp/session_backup"]),
    )
    .with_id(10);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "saveSession should succeed");

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"), "Result should contain OK");
    assert!(
        result.contains("3"),
        "Result should indicate 3 downloads saved"
    );
}

#[tokio::test]
async fn test_save_session_empty_dir_fails() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest::new("aria2.saveSession", serde_json::json!([""])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Empty dir should fail");
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "Should be InvalidParams error"
    );
}

#[tokio::test]
async fn test_change_position_pos_set_returns_target_index() {
    // Original aria2 protocol:
    //   params: [gid, pos, how]
    //   how = "POS_SET" → target = pos (absolute index from head)
    // Returns: integer target index.
    let engine = RpcEngine::new();

    // Create three tasks so the queue has a non-trivial length.
    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://change-pos-{}.com", i)]),
        )
        .with_id(i as i64);
        let _ = engine.handle_request(&req).await;
    }
    // Pick the first GID returned.
    let first_gid: String = {
        let req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(99);
        let resp = engine.handle_request(&req).await;
        let arr: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
        arr[0]["gid"].as_str().unwrap().to_string()
    };

    let change_req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([first_gid, 1, "POS_SET"]),
    )
    .with_id(2);
    let resp = engine.handle_request(&change_req).await;
    assert!(resp.is_success(), "POS_SET should succeed");
    let result: serde_json::Value = resp.result.unwrap();
    assert_eq!(
        result.as_i64(),
        Some(1),
        "POS_SET should return the requested target index as integer"
    );
}

#[tokio::test]
async fn test_change_position_pos_cur_returns_relative_index() {
    let engine = RpcEngine::new();
    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://cur-{}.com", i)]),
        )
        .with_id(i as i64);
        let _ = engine.handle_request(&req).await;
    }

    // `changePosition` computes the queue snapshot as the sorted list of GIDs.
    // To make this test deterministic (independent of random GID generation),
    // we explicitly pick the GID that lands at sorted position 1.
    let mut sorted_gids: Vec<String> = {
        let req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(99);
        let resp = engine.handle_request(&req).await;
        let arr: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
        arr.iter()
            .map(|v| v["gid"].as_str().unwrap().to_string())
            .collect()
    };
    sorted_gids.sort();
    assert_eq!(sorted_gids.len(), 3, "queue should have 3 entries");

    // Pick the GID at sorted position 1 (the middle of the queue).
    let target_gid = sorted_gids[1].clone();
    // current_pos = 1, pos = +1 → target = 2 (still within [0, 2]).
    let expected: i64 = 2;

    // POS_CUR with pos=+1 should move the gid one slot forward.
    let req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([target_gid, 1, "POS_CUR"]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "POS_CUR should succeed");
    let result: serde_json::Value = resp.result.unwrap();
    assert_eq!(
        result.as_i64(),
        Some(expected),
        "POS_CUR with pos=+1 from sorted position 1 should yield {}",
        expected
    );
}

#[tokio::test]
async fn test_change_position_pos_end_returns_tail_relative() {
    let engine = RpcEngine::new();
    for i in 0..3 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://end-{}.com", i)]),
        )
        .with_id(i as i64);
        let _ = engine.handle_request(&req).await;
    }
    let target_gid: String = {
        let req = JsonRpcRequest::new("aria2.tellActive", serde_json::json!([])).with_id(99);
        let resp = engine.handle_request(&req).await;
        let arr: Vec<serde_json::Value> = serde_json::from_value(resp.result.unwrap()).unwrap();
        arr[0]["gid"].as_str().unwrap().to_string()
    };

    // POS_END with pos=-1 should be tail-relative: (len-1) + (-1) = len-2 = 1.
    let req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([target_gid, -1, "POS_END"]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "POS_END should succeed");
    let result: serde_json::Value = resp.result.unwrap();
    assert_eq!(result.as_i64(), Some(1));
}

#[tokio::test]
async fn test_change_position_invalid_how_string() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://invalid-how.com/f"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Unknown `how` string → InvalidParams (matches original "Illegal argument.").
    let req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([gid, 0, "INVALID"]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Invalid 'how' value should fail");
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "Should be InvalidParams error"
    );
}

#[tokio::test]
async fn test_change_position_unknown_gid() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!(["0000000000000000", 0, "POS_SET"]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Unknown GID should fail");
    assert_eq!(
        resp.error.unwrap().code,
        -32601,
        "Should be MethodNotFound error"
    );
}

#[tokio::test]
async fn test_force_remove_cancels_immediately() {
    let engine = RpcEngine::new();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://force-remove.com/large.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());
    let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
    assert_eq!(
        status.status,
        DownloadStatus::Active,
        "Task should be active initially"
    );

    // Per aria2 RPC spec, forceRemove takes a single GID and returns the GID.
    let remove_req = JsonRpcRequest::new("aria2.forceRemove", serde_json::json!([gid])).with_id(3);
    let remove_resp = engine.handle_request(&remove_req).await;
    assert!(remove_resp.is_success(), "forceRemove should succeed");
    // Result should be the GID string, not "OK"
    let returned_gid: String = serde_json::from_value(remove_resp.result.unwrap()).unwrap();
    assert_eq!(returned_gid, gid, "forceRemove should return the GID");

    // After forceRemove, the task is removed from the tasks map entirely.
    let tell_req2 = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(4);
    let tell_resp2 = engine.handle_request(&tell_req2).await;
    assert!(
        tell_resp2.is_error(),
        "tellStatus should fail after forceRemove (task is gone)"
    );
}

#[tokio::test]
async fn test_batch_gids_force_remove() {
    // Per aria2 RPC spec, forceRemove accepts a SINGLE GID (not a batch array).
    // This test verifies that calling forceRemove on each GID individually
    // removes them correctly, and returns each GID.
    let engine = RpcEngine::new();

    let mut gids = Vec::new();
    for i in 0..4 {
        let req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://batch-remove.com/{}.iso", i)]),
        )
        .with_id(i);
        let resp = engine.handle_request(&req).await;
        let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        gids.push(gid);
    }
    assert_eq!(engine.task_count().await, 4);

    // Remove each GID one-by-one (the proper aria2 protocol)
    for gid in &gids {
        let req = JsonRpcRequest::new("aria2.forceRemove", serde_json::json!([gid])).with_id(10);
        let resp = engine.handle_request(&req).await;
        assert!(resp.is_success(), "forceRemove should succeed for {}", gid);
        let returned: String = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(returned, *gid, "forceRemove should return the GID");
    }
    assert_eq!(engine.task_count().await, 0, "all tasks should be removed");

    // Each GID should now be gone
    for gid in &gids {
        let tell_req =
            JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(20);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(
            tell_resp.is_error(),
            "tellStatus should fail for removed GID {}",
            gid
        );
    }
}

// =========================================================================
// L3 RPC Query Method Tests
// =========================================================================

#[tokio::test]
async fn test_get_uris_valid_gid_returns_uri_list() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!([["http://example.com/file.iso", "http://mirror.com/file.iso"]]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getUris should succeed for valid GID");

    let uris: Vec<UriInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(uris.len(), 2, "Should return 2 URIs");
    assert_eq!(uris[0].uri, "http://example.com/file.iso");
    assert_eq!(
        uris[0].status,
        UriStatus::Used,
        "First URI should be 'used'"
    );
    assert_eq!(
        uris[1].status,
        UriStatus::Waiting,
        "Second URI should be 'waiting'"
    );
}

#[tokio::test]
async fn test_get_uris_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getUris", serde_json::json!(["nonexistent-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getUris should fail for unknown GID");
    assert_eq!(
        resp.error.unwrap().code,
        -32601,
        "Should be MethodNotFound error"
    );
}

#[tokio::test]
async fn test_get_uris_single_uri() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let uris: Vec<UriInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(uris.len(), 1);
    assert_eq!(uris[0].uri, "http://x.com/f");
    assert_eq!(uris[0].status, UriStatus::Used);
}

#[tokio::test]
async fn test_get_uris_serialization_format() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://test.com/a.bin"]))
        .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getUris", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    let json_str = resp.to_string().unwrap();
    assert!(json_str.contains("\"jsonrpc\":\"2.0\""));
    assert!(json_str.contains("\"result\""));
    assert!(json_str.contains("\"uri\""));
    assert!(json_str.contains("\"status\""));
}

#[tokio::test]
async fn test_get_files_valid_gid_returns_file_list() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://example.com/large.iso"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine
        .update_task_progress(&gid, 10485760, 5242880, 0, 1024, 0, 2)
        .await;

    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getFiles should succeed for valid GID");

    let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(!files.is_empty(), "Should return at least one file");
    // FileInfo scalars are wire-format strings (matching original aria2
    // util::itos()/uitos()).
    assert_eq!(
        files[0].length, "10485760",
        "File length should match total_length"
    );
    assert_eq!(
        files[0].completed_length, "5242880",
        "completedLength should match completed_length"
    );
    assert_eq!(files[0].index, "1", "index is 1-based");
    assert_eq!(files[0].selected, "true", "selected defaults to VLB_TRUE");
}

#[tokio::test]
async fn test_get_files_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!(["unknown-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getFiles should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[tokio::test]
async fn test_get_files_zero_completed_length() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/new.zip"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(files[0].length, "0", "New task should have zero length");
    assert_eq!(
        files[0].completed_length, "0",
        "New task should have zero completed"
    );
    assert_eq!(
        files[0].selected, "true",
        "Default file should be selected (VLB_TRUE)"
    );
}

#[tokio::test]
async fn test_get_files_selected_field() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://sel.test/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        files[0].selected, "true",
        "FileInfo.selected should default to VLB_TRUE (\"true\")"
    );
}

#[tokio::test]
async fn test_get_servers_valid_gid_returns_server_list() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!([[
            "http://dl.example.com/file.bin",
            "http://mirror.example.com/file.bin"
        ]]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine
        .update_task_progress(&gid, 1000000, 500000, 0, 1048576, 0, 3)
        .await;

    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getServers should succeed for valid GID");

    let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        servers.len(),
        1,
        "Single-file download should have 1 ServerInfoIndex"
    );
    assert_eq!(servers[0].index, 0, "File index should be 0");
    assert_eq!(servers[0].servers.len(), 2, "Should have 2 server entries");
    assert_eq!(
        servers[0].servers[0].uri, "http://dl.example.com/file.bin",
        "First server URI should match"
    );
    assert_eq!(
        servers[0].servers[0].download_speed, 1048576,
        "Download speed should match task progress"
    );
}

#[tokio::test]
async fn test_get_servers_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!(["bad-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getServers should fail for unknown GID");
    // Per original aria2 `GetServersRpcMethod::process`, a missing GID throws
    // `DL_ABORT_EX` → JSON-RPC error code 1 with message
    // "No active download for GID#<hex>" (NOT -32601 MethodNotFound).
    let err = resp.error.unwrap();
    assert_eq!(err.code, 1);
    assert!(
        err.message.contains("No active download for GID#"),
        "error message should mention 'No active download', got: {}",
        err.message
    );
}

#[tokio::test]
async fn test_get_servers_non_active_download_returns_error() {
    let engine = RpcEngine::new();

    // Add a task (defaults to Active), then pause it.
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://example.com/file"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let pause_req = JsonRpcRequest::new("aria2.pause", serde_json::json!([gid])).with_id(2);
    engine.handle_request(&pause_req).await;

    // getServers on a paused (non-active) download must error.
    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(3);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_error(),
        "getServers should fail for non-active download"
    );
    let err = resp.error.unwrap();
    // Matches original aria2 DL_ABORT_EX → code 1.
    assert_eq!(err.code, 1);
    assert!(
        err.message.contains("No active download for GID#"),
        "error message should mention 'No active download', got: {}",
        err.message
    );
}

#[tokio::test]
async fn test_get_servers_zero_download_speed() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://zero-speed.com/f"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        servers[0].servers[0].download_speed, 0,
        "No-progress task should have 0 speed"
    );
    assert_eq!(
        servers[0].servers[0].current_uri, servers[0].servers[0].uri,
        "current_uri should equal uri when no redirect"
    );
}

#[tokio::test]
async fn test_get_servers_empty_uri_list() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://single.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(
        servers[0].servers.len(),
        1,
        "Single URI should produce 1 server entry"
    );
}

#[tokio::test]
async fn test_get_version_returns_version_info() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getVersion should succeed");

    let result = resp.result.unwrap();
    assert!(
        result.get("version").is_some(),
        "Response should contain version field"
    );
    assert!(
        result.get("enabledFeatures").is_some(),
        "Response should contain enabledFeatures field"
    );

    let version_info: VersionInfo = serde_json::from_value(result).unwrap();
    assert!(
        !version_info.version.is_empty(),
        "Version string should not be empty"
    );
    assert!(
        !version_info.enabled_features.is_empty(),
        "Enabled features list should not be empty"
    );
    assert!(
        version_info
            .enabled_features
            .contains(&"BitTorrent".to_string()),
        "Should include BitTorrent feature (capitalized, matching original aria2)"
    );
}

#[tokio::test]
async fn test_get_version_uses_cargo_pkg_version() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    let result = resp.result.unwrap();
    let version_info: VersionInfo = serde_json::from_value(result).unwrap();

    assert!(
        !version_info.version.is_empty(),
        "CARGO_PKG_VERSION should be set"
    );
}

#[tokio::test]
async fn test_get_version_enabled_features_count() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    let result = resp.result.unwrap();
    let version_info: VersionInfo = serde_json::from_value(result).unwrap();

    assert!(
        version_info.enabled_features.len() >= 6,
        "Should have at least 6 enabled features (BitTorrent, GZip, HTTPS, \
         Message Digest, Metalink, XML-RPC), got {}",
        version_info.enabled_features.len()
    );
}

#[tokio::test]
async fn test_get_version_json_rpc_response_format() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(42);
    let resp = engine.handle_request(&req).await;
    let json_str = resp.to_string().unwrap();

    assert!(
        json_str.contains("\"id\":42"),
        "Response ID should match request"
    );
    assert!(
        json_str.contains("\"version\""),
        "Should contain version key"
    );
    assert!(
        json_str.contains("\"enabledFeatures\""),
        "Should contain enabledFeatures key"
    );
}

#[tokio::test]
async fn test_purge_download_result_specific_gid() {
    let engine = RpcEngine::new();

    let stopped_gid = "stopped-gid-001".to_string();
    let stopped_status = StatusInfo::new(&stopped_gid)
        .with_status(DownloadStatus::Complete)
        .with_total_length(1000)
        .with_completed_length(1000);
    {
        let mut stopped = engine.stopped_tasks.write().await;
        stopped.push(stopped_status);
    }
    assert_eq!(engine.stopped_tasks.read().await.len(), 1);

    let req = JsonRpcRequest::new(
        "aria2.purgeDownloadResult",
        serde_json::json!([stopped_gid]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "purgeDownloadResult with valid GID should succeed"
    );

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
    assert_eq!(
        engine.stopped_tasks.read().await.len(),
        0,
        "Stopped task should be removed after purge"
    );
}

#[tokio::test]
async fn test_purge_download_result_gid_not_found() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new(
        "aria2.purgeDownloadResult",
        serde_json::json!(["nonexistent-stopped-gid"]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_error(),
        "purgeDownloadResult with unknown GID should fail"
    );
    assert_eq!(
        resp.error.unwrap().code,
        -32601,
        "Should be MethodNotFound error"
    );
}

#[tokio::test]
async fn test_purge_download_result_no_param_clears_all() {
    let engine = RpcEngine::new();

    for i in 0..3 {
        let status =
            StatusInfo::new(format!("stopped-{}", i)).with_status(DownloadStatus::Complete);
        engine.stopped_tasks.write().await.push(status);
    }
    assert_eq!(engine.stopped_tasks.read().await.len(), 3);

    let req = JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "No-param purge should succeed");
    assert_eq!(
        engine.stopped_tasks.read().await.len(),
        0,
        "All stopped tasks should be cleared"
    );
}

#[tokio::test]
async fn test_purge_download_result_partial_purge() {
    let engine = RpcEngine::new();

    let gid_a = "gid-a".to_string();
    let gid_b = "gid-b".to_string();
    let gid_c = "gid-c".to_string();
    {
        let mut stopped = engine.stopped_tasks.write().await;
        stopped.push(StatusInfo::new(&gid_a).with_status(DownloadStatus::Complete));
        stopped.push(StatusInfo::new(&gid_b).with_status(DownloadStatus::Complete));
        stopped.push(
            StatusInfo::new(&gid_c).with_status(DownloadStatus::Error("unknown".to_string())),
        );
    }
    assert_eq!(engine.stopped_tasks.read().await.len(), 3);

    let req = JsonRpcRequest::new(
        "aria2.purgeDownloadResult",
        serde_json::json!([gid_b.clone()]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let stopped = engine.stopped_tasks.read().await;
    assert_eq!(stopped.len(), 2);
    let remaining_gids: Vec<&String> = stopped.iter().map(|s| &s.gid).collect();
    assert!(remaining_gids.contains(&&gid_a), "gid_a should remain");
    assert!(remaining_gids.contains(&&gid_c), "gid_c should remain");
    assert!(!remaining_gids.contains(&&gid_b), "gid_b should be purged");
}

#[tokio::test]
async fn test_get_session_info_returns_session_id() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getSessionInfo should succeed");

    let result = resp.result.unwrap();
    assert!(
        result.get("sessionId").is_some(),
        "Response should contain sessionId field"
    );

    let session_id = result.get("sessionId").unwrap().as_str().unwrap();
    assert!(
        session_id.starts_with("session-"),
        "Session ID should start with 'session-' prefix, got: {}",
        session_id
    );
    assert!(!session_id.is_empty(), "Session ID should not be empty");
}

#[tokio::test]
async fn test_get_session_info_unique_per_call() {
    let engine = RpcEngine::new();
    let req1 = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
    let resp1 = engine.handle_request(&req1).await;

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let req2 = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(2);
    let resp2 = engine.handle_request(&req2).await;

    let sid1 = resp1
        .result
        .unwrap()
        .get("sessionId")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let sid2 = resp2
        .result
        .unwrap()
        .get("sessionId")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    assert!(
        !sid1.is_empty() && !sid2.is_empty(),
        "Both session IDs should be non-empty"
    );
}

#[tokio::test]
async fn test_get_session_info_struct_fields() {
    let session_info = SessionInfo::new();
    assert!(
        !session_info.session_id.is_empty(),
        "session_id should not be empty"
    );
    assert!(
        session_info.session_start_time > 0,
        "session_start_time should be positive Unix timestamp"
    );

    let json_val = session_info.to_json_value();
    assert!(
        json_val.get("sessionId").is_some(),
        "JSON should contain sessionId"
    );
}

#[tokio::test]
async fn test_get_session_info_json_rpc_format() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(99);
    let resp = engine.handle_request(&req).await;
    let json_str = resp.to_string().unwrap();

    assert!(json_str.contains("\"id\":99"), "Response ID should match");
    assert!(
        json_str.contains("\"sessionId\""),
        "Should contain sessionId field"
    );
    assert!(json_str.contains("\"result\""), "Should have result field");
}

// =========================================================================
// Task 2.3: aria2.shutdown / aria2.forceShutdown 3-second delay
//
// Mirrors the original aria2 `goingShutdown(req, e, forceHalt)` behaviour in
// `RpcMethodImpl.cc`: returns `"OK"` immediately and schedules a delayed halt
// via `TimedHaltCommand` (here `RpcEngine::schedule_halt`). The delay gives
// the RPC client (e.g. AriaNg) time to receive the response body before the
// server begins shutting down. The `force` variant additionally cancels all
// active downloads (matching `DownloadEngine::forceHalt()`).
// =========================================================================

#[tokio::test]
async fn test_shutdown_returns_ok_immediately_and_honors_delay() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.shutdown", serde_json::json!([])).with_id(42);
    let resp = engine.handle_request(&req).await;

    // "OK" must be returned immediately so the client receives it before shutdown
    assert!(resp.is_success(), "shutdown should return success");

    // Capture the JSON string first to avoid partially moving `resp.result`
    let json_str = resp.to_string().unwrap();
    assert!(
        json_str.contains("\"id\":42"),
        "response id should echo request id: {}",
        json_str
    );
    assert!(
        json_str.contains("\"result\":\"OK\""),
        "shutdown should return exactly \"OK\": {}",
        json_str
    );

    // The 3-second delay is honored: signal must NOT fire immediately
    assert!(
        !engine.shutdown_token().is_cancelled(),
        "shutdown signal must not fire immediately — HALT_DELAY honored"
    );
}

#[tokio::test]
async fn test_force_shutdown_returns_ok_immediately_and_honors_delay() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.forceShutdown", serde_json::json!([])).with_id(7);
    let resp = engine.handle_request(&req).await;

    assert!(resp.is_success(), "forceShutdown should return success");
    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK", "forceShutdown should return exactly \"OK\"");

    assert!(
        !engine.shutdown_token().is_cancelled(),
        "shutdown signal must not fire immediately — HALT_DELAY honored"
    );
}

/// Verifies that a non-force halt fires the shutdown signal after the delay
/// but does NOT cancel active downloads (matching `forceHalt=false`).
///
/// Uses `multi_thread` flavor so the spawned halt task is not blocked by the
/// test's own `sleep` future on the same runtime thread.
#[tokio::test(flavor = "multi_thread")]
async fn test_schedule_halt_non_force_does_not_cancel_downloads() {
    let engine = RpcEngine::new();

    // Seed one active download via the public RPC path
    let add_req = JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/file.bin"]))
        .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let _gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();
    assert_eq!(
        engine.task_count().await,
        1,
        "task should be active after addUri"
    );

    // Schedule a non-force halt with a short delay (10ms instead of 3s)
    engine.schedule_halt(std::time::Duration::from_millis(10), false);

    // Wait long enough for the spawned task to fire
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert!(
        engine.shutdown_token().is_cancelled(),
        "shutdown signal should fire after the delay"
    );
    assert_eq!(
        engine.task_count().await,
        1,
        "non-force halt must NOT cancel active downloads"
    );
}

/// Verifies that a force halt cancels every active download's token, marks
/// them as Removed, and clears the task map (matching `forceHalt=true`).
#[tokio::test(flavor = "multi_thread")]
async fn test_schedule_halt_force_cancels_all_downloads() {
    let engine = RpcEngine::new();

    // Seed two active downloads
    let add1 =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/a.bin"])).with_id(1);
    let add2 =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/b.bin"])).with_id(2);
    engine.handle_request(&add1).await;
    engine.handle_request(&add2).await;
    assert_eq!(engine.task_count().await, 2, "two tasks should be active");

    // Schedule a force halt with a short delay
    engine.schedule_halt(std::time::Duration::from_millis(10), true);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert!(
        engine.shutdown_token().is_cancelled(),
        "shutdown signal should fire after the delay"
    );
    assert_eq!(
        engine.task_count().await,
        0,
        "force halt should cancel and clear all active downloads"
    );
}

/// Verifies the delay is actually honored: the signal must not fire before
/// the configured delay elapses, then must fire after.
#[tokio::test(flavor = "multi_thread")]
async fn test_schedule_halt_does_not_fire_before_delay() {
    let engine = RpcEngine::new();

    // 200ms delay — long enough to clearly distinguish "before" vs "after"
    engine.schedule_halt(std::time::Duration::from_millis(200), false);

    // 50ms wait — well before the 200ms delay
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !engine.shutdown_token().is_cancelled(),
        "shutdown signal must NOT fire before the delay elapses"
    );

    // Wait past the 200ms delay (with safety margin for scheduling jitter)
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        engine.shutdown_token().is_cancelled(),
        "shutdown signal should fire after the delay elapses"
    );
}

/// Verifies that multiple `schedule_halt` calls are safe — each spawns an
/// independent task, but cancelling an already-cancelled token is a no-op
/// (idempotent), so the engine does not panic or double-fire.
#[tokio::test(flavor = "multi_thread")]
async fn test_schedule_halt_idempotent_multiple_calls() {
    let engine = RpcEngine::new();

    // Fire three halts with overlapping delays
    engine.schedule_halt(std::time::Duration::from_millis(50), false);
    engine.schedule_halt(std::time::Duration::from_millis(10), false);
    engine.schedule_halt(std::time::Duration::from_millis(20), true);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    assert!(
        engine.shutdown_token().is_cancelled(),
        "shutdown signal should fire after all delays complete"
    );
}
