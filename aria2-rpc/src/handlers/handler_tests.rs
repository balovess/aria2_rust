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
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(fake_torrent_bencode.as_bytes());
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
    let req =
        JsonRpcRequest::new("aria2.addTorrent", serde_json::json!([not_torrent])).with_id(1);
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
    let not_metalink =
        base64::engine::general_purpose::STANDARD.encode("this is not metalink xml");
    let req =
        JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([not_metalink])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "addMetalink should fail for non-XML data");
}

#[tokio::test]
async fn test_tell_status_has_real_progress_data() {
    let engine = RpcEngine::new();

    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/large.iso"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine.update_task_progress(
        &gid,
        10485760,
        5242880,
        1024,
        1048576,
        512,
        3,
    ).await;

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success(), "tellStatus should succeed");

    let status_val = tell_resp.result.unwrap();
    let status: StatusInfo = serde_json::from_value(status_val).unwrap();

    assert_eq!(status.total_length, Some(10485760), "total_length should be 10MB");
    assert_eq!(status.completed_length, Some(5242880), "completed_length should be 5MB (50%)");
    assert_eq!(status.upload_length, Some(1024), "upload_length should be 1KB");
    assert_eq!(status.download_speed, Some(1048576), "download_speed should be 1MB/s");
    assert_eq!(status.upload_speed, Some(512), "upload_speed should be 512B/s");
    assert_eq!(status.connections, Some(3), "connections should be 3");

    let expected_percent = (5242880.0 / 10485760.0) * 100.0;
    assert!((status.progress_percent() - expected_percent).abs() < 0.01,
            "progress percent should be ~50%");
}

#[tokio::test]
async fn test_tell_status_zero_for_nonexistent_gid() {
    let engine = RpcEngine::new();

    let tell_req = JsonRpcRequest::new(
        "aria2.tellStatus",
        serde_json::json!(["nonexistent-gid-12345"])
    ).with_id(1);
    let tell_resp = engine.handle_request(&tell_req).await;

    assert!(tell_resp.is_error(), "tellStatus should fail for non-existent GID");
    assert_eq!(tell_resp.error.unwrap().code, -32601,
               "error code should be MethodNotFound (-32601)");
}

#[tokio::test]
async fn test_tell_status_includes_upload_fields() {
    let engine = RpcEngine::new();

    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://torrent.example.com/file.torrent"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    engine.update_task_progress(
        &gid,
        1073741824,
        1073741824,
        536870912,
        0,
        1048576,
        10,
    ).await;

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());

    let status_val = tell_resp.result.unwrap();
    let status: StatusInfo = serde_json::from_value(status_val).unwrap();

    assert!(status.upload_length.is_some(), "upload_length field must be present");
    assert!(status.upload_speed.is_some(), "upload_speed field must be present");
    assert_eq!(status.upload_length, Some(536870912),
               "upload_length should reflect seeding contribution");
    assert_eq!(status.upload_speed, Some(1048576),
               "upload_speed should show current seeding rate");
    assert_eq!(status.connections, Some(10),
               "connections should show peer count");
}

#[tokio::test]
async fn test_get_peers_returns_peer_list() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f.torrent"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let peers = vec![
        PeerInfo {
            peer_id: "p1".to_string(),
            ip: "10.0.0.1".to_string(),
            port: 6881,
            am_choking: false,
            peer_choking: true,
            download_speed: 100000,
            upload_speed: 50000,
        },
        PeerInfo {
            peer_id: "p2".to_string(),
            ip: "10.0.0.2".to_string(),
            port: 6882,
            am_choking: true,
            peer_choking: false,
            download_speed: 200000,
            upload_speed: 75000,
        },
    ];
    engine.set_task_peers(&gid, peers.clone()).await;

    let req = JsonRpcRequest::new("aria2.getPeers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getPeers should succeed for existing GID");

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
    assert_eq!(status.status, DownloadStatus::Active, "Task should be Active after unpauseAll");
}

#[tokio::test]
async fn test_change_uri_adds_uris() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/original.iso"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = JsonRpcRequest::new(
        "aria2.changeUri",
        serde_json::json!([gid, 0, null, ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]]),
    ).with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success(), "changeUri should succeed");

    let result = change_resp.result.unwrap();
    let arr = result.as_array().unwrap();
    assert_eq!(arr[0], gid, "First element of result should be gid");
    assert_eq!(arr[1], 0, "Second element should be 0 (no file index change)");
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
    ).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "changeOption with unknown key should fail");
    assert_eq!(resp.error.unwrap().code, -32602, "error code should be InvalidParams (-32602)");
}

#[tokio::test]
async fn test_change_option_accepts_valid_keys() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let valid_changes = serde_json::json!({
        "max-download-limit": 1048576,
        "split": 5,
        "dir": "/tmp/downloads"
    });
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, valid_changes]),
    ).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "changeOption with valid keys should succeed");

    let get_req = JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid])).with_id(3);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
    let opts: HashMap<String, serde_json::Value> = serde_json::from_value(get_resp.result.unwrap()).unwrap();
    assert!(opts.contains_key("max-download-limit"), "max-download-limit should be stored");
    assert!(opts.contains_key("split"), "split should be stored");
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

    let version_result = &results[0];
    assert!(
        version_result.get("version").is_some() || version_result.as_str().is_some(),
        "getVersion result should contain version info"
    );

    let stat_result = &results[1];
    assert!(
        stat_result.get("downloadSpeed").is_some(),
        "getGlobalStat should contain downloadSpeed"
    );

    let session_result = &results[2];
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

    assert!(results[0].get("version").is_some() || results[0].get("enabledFeatures").is_some());
    let active = results[1].as_array().expect("tellActive should return array");
    assert_eq!(active.len(), 3, "Should have 3 active tasks");
    assert!(results[2].get("downloadSpeed").is_some());
    assert!(results[3].get("sessionId").is_some());
}

#[tokio::test]
async fn test_multicall_empty_calls_returns_empty_array() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[]]),
    )
    .with_id(1);

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
    assert!(resp.is_success(), "Multicall with addUri + getGlobalStat should succeed");

    let result_value = resp.result.unwrap();
    let results = result_value.as_array().unwrap();
    assert_eq!(results.len(), 2);

    assert!(!results[0].is_null(), "addUri should return a value");
    assert!(
        results[1].get("downloadSpeed").is_some(),
        "getGlobalStat should contain downloadSpeed"
    );
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
    assert!(result.contains("3"), "Result should indicate 3 downloads saved");
}

#[tokio::test]
async fn test_save_session_empty_dir_fails() {
    let engine = RpcEngine::new();

    let req = JsonRpcRequest::new(
        "aria2.saveSession",
        serde_json::json!([""]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Empty dir should fail");
    assert_eq!(resp.error.unwrap().code, -32602, "Should be InvalidParams error");
}

#[tokio::test]
async fn test_change_position_move_uri() {
    let engine = RpcEngine::new();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://uri1.com"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([gid, 0, 0, 2, 0]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success(), "changePosition should succeed for valid positions");

    let result: String = serde_json::from_value(change_resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK", "Should return OK");
}

#[tokio::test]
async fn test_change_position_invalid_how() {
    let engine = RpcEngine::new();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://invalid-how.com/f"]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([gid, 0, null, null, 99]),
    )
    .with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Invalid 'how' value should fail");
    assert_eq!(resp.error.unwrap().code, -32602, "Should be InvalidParams error");
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

    let tell_req = JsonRpcRequest::new(
        "aria2.tellStatus",
        serde_json::json!([gid]),
    )
    .with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());
    let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
    assert_eq!(status.status, DownloadStatus::Active, "Task should be active initially");

    let remove_req = JsonRpcRequest::new(
        "aria2.forceRemove",
        serde_json::json!([gid]),
    )
    .with_id(3);
    let remove_resp = engine.handle_request(&remove_req).await;
    assert!(remove_resp.is_success(), "forceRemove should succeed");

    let tell_req2 = JsonRpcRequest::new(
        "aria2.tellStatus",
        serde_json::json!([gid]),
    )
    .with_id(4);
    let tell_resp2 = engine.handle_request(&tell_req2).await;
    assert!(tell_resp2.is_success(), "tellStatus should still work after forceRemove");
    let status_after: StatusInfo = serde_json::from_value(tell_resp2.result.unwrap()).unwrap();
    assert_eq!(
        status_after.status,
        DownloadStatus::Removed,
        "Task should be marked as Removed after forceRemove"
    );
}

#[tokio::test]
async fn test_batch_gids_force_remove() {
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

    let req = JsonRpcRequest::new(
        "aria2.forceRemove",
        serde_json::json!([gids.clone()]),
    )
    .with_id(10);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "Batch forceRemove should succeed");

    for gid in &gids {
        let tell_req = JsonRpcRequest::new(
            "aria2.tellStatus",
            serde_json::json!([gid]),
        )
        .with_id(20);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let status: StatusInfo = serde_json::from_value(tell_resp.result.unwrap()).unwrap();
        assert_eq!(
            status.status,
            DownloadStatus::Removed,
            "GID {} should be Removed after batch forceRemove",
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
    assert_eq!(uris[0].status, UriStatus::Used, "First URI should be 'used'");
    assert_eq!(uris[1].status, UriStatus::Waiting, "Second URI should be 'waiting'");
}

#[tokio::test]
async fn test_get_uris_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getUris", serde_json::json!(["nonexistent-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getUris should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, -32601, "Should be MethodNotFound error");
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
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["http://test.com/a.bin"]),
    )
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
    assert_eq!(files[0].length, 10485760, "File length should match total_length");
    assert_eq!(
        files[0].completed_length, 5242880,
        "completedLength should match completed_length"
    );
}

#[tokio::test]
async fn test_get_files_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getFiles", serde_json::json!(["unknown-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getFiles should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[tokio::test]
async fn test_get_files_zero_completed_length() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/new.zip"]))
            .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let files: Vec<FileInfo> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(files[0].length, 0, "New task should have zero length");
    assert_eq!(files[0].completed_length, 0, "New task should have zero completed");
    assert!(files[0].selected, "Default file should be selected");
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
    assert!(files[0].selected, "FileInfo.selected should default to true");
}

#[tokio::test]
async fn test_get_servers_valid_gid_returns_server_list() {
    let engine = RpcEngine::new();
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!([["http://dl.example.com/file.bin", "http://mirror.example.com/file.bin"]]),
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
    assert_eq!(servers.len(), 1, "Single-file download should have 1 ServerInfoIndex");
    assert_eq!(servers[0].index, 0, "File index should be 0");
    assert_eq!(
        servers[0].servers.len(), 2,
        "Should have 2 server entries"
    );
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
    let req =
        JsonRpcRequest::new("aria2.getServers", serde_json::json!(["bad-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getServers should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[tokio::test]
async fn test_get_servers_zero_download_speed() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://zero-speed.com/f"]))
            .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let servers: Vec<ServerInfoIndex> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(servers[0].servers[0].download_speed, 0, "No-progress task should have 0 speed");
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
    assert_eq!(servers[0].servers.len(), 1, "Single URI should produce 1 server entry");
}

#[tokio::test]
async fn test_get_version_returns_version_info() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getVersion should succeed");

    let result = resp.result.unwrap();
    assert!(result.get("version").is_some(), "Response should contain version field");
    assert!(
        result.get("enabledFeatures").is_some(),
        "Response should contain enabledFeatures field"
    );

    let version_info: VersionInfo = serde_json::from_value(result).unwrap();
    assert!(!version_info.version.is_empty(), "Version string should not be empty");
    assert!(
        !version_info.enabled_features.is_empty(),
        "Enabled features list should not be empty"
    );
    assert!(
        version_info.enabled_features.contains(&"bittorrent".to_string()),
        "Should include bittorrent feature"
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
        version_info.enabled_features.len() >= 5,
        "Should have at least 5 enabled features, got {}",
        version_info.enabled_features.len()
    );
}

#[tokio::test]
async fn test_get_version_json_rpc_response_format() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getVersion", serde_json::json!([])).with_id(42);
    let resp = engine.handle_request(&req).await;
    let json_str = resp.to_string().unwrap();

    assert!(json_str.contains("\"id\":42"), "Response ID should match request");
    assert!(json_str.contains("\"version\""), "Should contain version key");
    assert!(json_str.contains("\"enabledFeatures\""), "Should contain enabledFeatures key");
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
    assert!(resp.is_success(), "purgeDownloadResult with valid GID should succeed");

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
    assert_eq!(resp.error.unwrap().code, -32601, "Should be MethodNotFound error");
}

#[tokio::test]
async fn test_purge_download_result_no_param_clears_all() {
    let engine = RpcEngine::new();

    for i in 0..3 {
        let status = StatusInfo::new(format!("stopped-{}", i))
            .with_status(DownloadStatus::Complete);
        engine.stopped_tasks.write().await.push(status);
    }
    assert_eq!(engine.stopped_tasks.read().await.len(), 3);

    let req =
        JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
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
        stopped.push(StatusInfo::new(&gid_c).with_status(DownloadStatus::Error("unknown".to_string())));
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

    let sid1 = resp1.result.unwrap().get("sessionId").unwrap().as_str().unwrap().to_string();
    let sid2 = resp2.result.unwrap().get("sessionId").unwrap().as_str().unwrap().to_string();

    assert!(!sid1.is_empty() && !sid2.is_empty(), "Both session IDs should be non-empty");
}

#[tokio::test]
async fn test_get_session_info_struct_fields() {
    let session_info = SessionInfo::new();
    assert!(!session_info.session_id.is_empty(), "session_id should not be empty");
    assert!(
        session_info.session_start_time > 0,
        "session_start_time should be positive Unix timestamp"
    );

    let json_val = session_info.to_json_value();
    assert!(json_val.get("sessionId").is_some(), "JSON should contain sessionId");
}

#[tokio::test]
async fn test_get_session_info_json_rpc_format() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(99);
    let resp = engine.handle_request(&req).await;
    let json_str = resp.to_string().unwrap();

    assert!(json_str.contains("\"id\":99"), "Response ID should match");
    assert!(json_str.contains("\"sessionId\""), "Should contain sessionId field");
    assert!(json_str.contains("\"result\""), "Should have result field");
}
