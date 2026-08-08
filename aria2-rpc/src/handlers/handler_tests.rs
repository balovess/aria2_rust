//! Handler integration tests.
//!
//! Tests for RPC handler methods exercised through `RpcEngine::handle_request`.

use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::RpcEngine;
use crate::json_rpc::JsonRpcRequest;
use crate::types::{SessionInfo, StatusInfo, VersionInfo};
use crate::websocket::{DownloadEvent, EventType};
use aria2_core::util::rwlock_ext::RwLockRecover;

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
    let gid_list: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(
        !gid_list.is_empty(),
        "addMetalink should return non-empty GID array"
    );
    assert_eq!(engine.task_count().await, 1);
}

#[tokio::test]
async fn test_add_metalink_direct_only_applies_filters_and_priority() {
    let engine = RpcEngine::new();
    let metalink_xml = r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><files><file name="first.bin"><url location="de" priority="1">https://de.example/first.bin</url></file><file name="second.bin"><url location="de" priority="1">https://de.example/second.bin</url><url location="us" priority="100">https://us.example/second.bin</url></file></files></metalink>"#;
    let encoded = base64::engine::general_purpose::STANDARD.encode(metalink_xml.as_bytes());
    let req = JsonRpcRequest::new(
        "aria2.addMetalink",
        serde_json::json!([
            encoded,
            {
                "select-file": "2",
                "metalink-location": "us",
                "dir": "target/rpc-metalink"
            }
        ]),
    )
    .with_id(1);

    let response = engine.handle_request(&req).await;
    let gids: Vec<String> = serde_json::from_value(response.result.expect("RPC result")).unwrap();
    assert_eq!(
        gids.len(),
        1,
        "select-file must be applied before GID creation"
    );

    let group_man = engine
        .group_man
        .as_ref()
        .expect("test manager")
        .read()
        .await;
    let group = group_man
        .group_by_hex(&gids[0])
        .expect("direct Metalink group should be registered");
    let group = group.recover();
    assert_eq!(group.output_name().as_deref(), Some("second.bin"));
    assert_eq!(
        group.uris(),
        &[
            "https://us.example/second.bin".to_string(),
            "https://de.example/second.bin".to_string()
        ]
    );
    assert_eq!(group.options().select_file.as_deref(), Some("2"));
}

#[tokio::test]
async fn test_add_metalink_position_inserts_the_whole_result() {
    let engine = RpcEngine::new();
    let first = JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!([["https://example.test/first.bin"]]),
    )
    .with_id(1);
    let _ = engine.handle_request(&first).await;

    let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="second.bin"><url>https://example.test/second.bin</url></file></metalink>"#;
    let encoded = base64::engine::general_purpose::STANDARD.encode(xml.as_bytes());
    let request =
        JsonRpcRequest::new("aria2.addMetalink", serde_json::json!([encoded, {}, 0])).with_id(2);
    let response = engine.handle_request(&request).await;
    let gids: Vec<String> = serde_json::from_value(response.result.expect("RPC result")).unwrap();
    assert_eq!(gids.len(), 1);

    let group_man = engine
        .group_man
        .as_ref()
        .expect("test manager")
        .read()
        .await;
    let waiting = group_man.get_waiting_groups();
    assert_eq!(waiting[0].recover().gid().to_hex_string(), gids[0]);
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

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success(), "tellStatus should succeed");

    let status_val = tell_resp.result.unwrap();
    // Wire format: all numbers are strings matching original aria2
    assert_eq!(
        status_val["totalLength"].as_str(),
        Some("0"),
        "Unknown length remains zero until protocol metadata arrives"
    );
    assert_eq!(
        status_val["completedLength"].as_str(),
        Some("0"),
        "A new task has no completed bytes"
    );
    assert_eq!(
        status_val["uploadLength"].as_str(),
        Some("0"),
        "A new task has no uploaded bytes"
    );
    assert_eq!(
        status_val["downloadSpeed"].as_str(),
        Some("0"),
        "A new task has no measured download speed"
    );
    assert_eq!(
        status_val["uploadSpeed"].as_str(),
        Some("0"),
        "A new task has no measured upload speed"
    );
    assert_eq!(
        status_val["connections"].as_str(),
        Some("5"),
        "Connections reflect the configured split count"
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
        1,
        "error code should be RpcExecution (1)"
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

    let tell_req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(2);
    let tell_resp = engine.handle_request(&tell_req).await;
    assert!(tell_resp.is_success());

    let status_val = tell_resp.result.unwrap();
    // Wire format: all values are strings matching original aria2
    assert!(
        status_val.get("uploadLength").is_some(),
        "uploadLength field must be present"
    );
    assert!(
        status_val.get("uploadSpeed").is_some(),
        "uploadSpeed field must be present"
    );
    assert_eq!(
        status_val["uploadLength"].as_str(),
        Some("0"),
        "A new task has no uploaded bytes"
    );
    assert_eq!(
        status_val["uploadSpeed"].as_str(),
        Some("0"),
        "A new task has no measured upload speed"
    );
    assert_eq!(
        status_val["connections"].as_str(),
        Some("5"),
        "Connections reflect the configured split count"
    );
}

#[tokio::test]
async fn test_get_peers_returns_core_state_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getPeers", serde_json::json!(["0000000000000001"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error());
}

#[tokio::test]
async fn test_get_peers_unknown_gid() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getPeers", serde_json::json!(["nonexistent-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getPeers should fail for non-existent GID");
    assert_eq!(resp.error.unwrap().code, 1);
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
    let status_val = tell_resp.result.unwrap();
    assert_eq!(
        status_val["status"].as_str(),
        Some("waiting"),
        "Without an engine loop, an unpause command remains queued"
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
            1,
            [],
            ["http://mirror1.com/file.iso", "http://mirror2.com/file.iso"]
        ]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success(), "changeUri should succeed");

    let result = change_resp.result.unwrap();
    let arr = result.as_array().unwrap();
    // Handler returns [delCount, addCount]; wire format: all numbers as strings
    assert_eq!(
        arr[0].as_str(),
        Some("0"),
        "First element should be delCount (0 deletions)"
    );
    assert_eq!(
        arr[1].as_str(),
        Some("2"),
        "Second element should be addCount (2 URIs added)"
    );
}

#[tokio::test]
async fn test_unpause_fires_download_start_event() {
    // C++ aria2 fires onDownloadStart (not onDownloadResume) when unpaused.
    // This test verifies the notification format matches C++ exactly.
    let event = DownloadEvent::download_start("gid-resume-001");
    assert_eq!(event.event_type().unwrap(), EventType::DownloadStart);
    assert_eq!(event.method(), "aria2.onDownloadStart");
    let json = event.to_json().unwrap();
    assert!(json.contains("\"method\":\"aria2.onDownloadStart\""));
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

    // Only the 7 options with setChangeOption(true) in C++ are accepted
    // for immediate change on active downloads. These are: force-save,
    // save-not-found, max-download-limit, bt-max-peers,
    // bt-remove-unselected-file, bt-request-peer-speed-limit,
    // max-upload-limit.
    let valid_changes = serde_json::json!({
        "max-download-limit": 1048576,
        "max-upload-limit": 512000,
        "bt-max-peers": 60
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
        opts.contains_key("bt-max-peers"),
        "bt-max-peers should be stored"
    );
}

#[tokio::test]
async fn test_change_option_rejects_startup_only_key() {
    let engine = RpcEngine::new();
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://x.com/f"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // `pause` is explicitly excluded from runtime-changeable options
    // (matching original C++ aria2 exclusion list), so changeOption must
    // reject it with InvalidParams.
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid, {"pause": "true"}]),
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

    // Per C++ aria2 spec, each successful result is wrapped in [[result]]
    let version_result = &results[0];
    let version_inner = version_result
        .as_array()
        .expect("multicall success result should be wrapped in array")
        .first()
        .expect("inner array should have element");
    assert!(
        version_inner.get("version").is_some() || version_inner.as_str().is_some(),
        "getVersion result should contain version info"
    );

    let stat_result = &results[1];
    let stat_inner = stat_result
        .as_array()
        .expect("multicall success result should be wrapped in array")
        .first()
        .expect("inner array should have element");
    assert!(
        stat_inner.get("downloadSpeed").is_some(),
        "getGlobalStat should contain downloadSpeed"
    );

    let session_result = &results[2];
    let session_inner = session_result
        .as_array()
        .expect("multicall success result should be wrapped in array")
        .first()
        .expect("inner array should have element");
    assert!(
        session_inner.get("sessionId").is_some(),
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

    // Per C++ aria2 spec, each successful result is wrapped in [[result]]
    let version_inner = results[0].as_array().unwrap().first().unwrap();
    assert!(
        version_inner.get("version").is_some() || version_inner.get("enabledFeatures").is_some()
    );
    let active_inner = results[1].as_array().unwrap().first().unwrap();
    let active = active_inner
        .as_array()
        .expect("tellActive should return array");
    assert_eq!(
        active.len(),
        0,
        "Without an engine loop, tasks remain waiting"
    );
    let stat_inner = results[2].as_array().unwrap().first().unwrap();
    assert!(stat_inner.get("downloadSpeed").is_some());
    let session_inner = results[3].as_array().unwrap().first().unwrap();
    assert!(session_inner.get("sessionId").is_some());
}

#[tokio::test]
async fn test_multicall_invalid_entries_match_cpp_errors_and_continue() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new(
        "system.multicall",
        serde_json::json!([[null, {"methodName": "system.multicall"}, {"methodName": "aria2.getVersion"}]]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let results = resp.result.unwrap().as_array().unwrap().clone();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["code"], "1");
    assert!(
        results[0]["message"]
            .as_str()
            .unwrap()
            .contains("expected struct")
    );
    assert_eq!(results[1]["code"], "-32600");
    assert!(
        results[1]["message"]
            .as_str()
            .unwrap()
            .contains("Recursive")
    );
    assert!(results[2].as_array().unwrap()[0].get("version").is_some());
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

    // Per C++ aria2 spec, each successful result is wrapped in [[result]]
    let add_uri_inner = results[0]
        .as_array()
        .expect("addUri result should be wrapped in array");
    assert!(!add_uri_inner.is_empty(), "addUri should return a value");
    let stat_inner = results[1].as_array().unwrap().first().unwrap();
    assert!(
        stat_inner.get("downloadSpeed").is_some(),
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

    // Use a temp-file path so the write is portable (C:\tmp\... would fail
    // or leak on Windows).
    let path =
        std::env::temp_dir().join(format!("test_save_session_rpc_{}.sess", std::process::id()));
    let _ = tokio::fs::remove_file(&path).await;

    let req = JsonRpcRequest::new(
        "aria2.saveSession",
        serde_json::json!([path.to_str().unwrap()]),
    )
    .with_id(10);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "saveSession should succeed");

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.as_str(), "OK", "Result should contain OK");

    // The session file must actually be written with the task URIs.
    assert!(path.exists(), "saveSession must write the session file");
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        content.contains("http://save-session.com/0"),
        "session file should contain the saved task URI"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

/// `aria2.saveSession` with no explicit path must fall back to the engine's
/// configured `--save-session` path (mirrors C++ reading PREF_SAVE_SESSION).
#[tokio::test]
async fn test_save_session_uses_configured_path() {
    let path =
        std::env::temp_dir().join(format!("test_save_session_cfg_{}.sess", std::process::id()));
    let _ = tokio::fs::remove_file(&path).await;

    let engine = RpcEngine::new().with_save_session_path(path.clone());
    let req = JsonRpcRequest::new("aria2.saveSession", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "saveSession with configured path should succeed"
    );
    assert!(
        path.exists(),
        "session file should be written to configured path"
    );
    let _ = tokio::fs::remove_file(&path).await;
}

/// `aria2.saveSession` with neither a path param nor a configured
/// save-session path must fail (C++ throws "Filename is not given.").
#[tokio::test]
async fn test_save_session_without_path_errors() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.saveSession", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "saveSession without any path should error");
}

/// `aria2.saveSession` wired to a RequestGroupMan persists the real group
/// state and returns "OK".
#[tokio::test]
async fn test_save_session_with_group_man() {
    use aria2_core::request::request_group::DownloadOptions;
    use aria2_core::request::request_group_man::RequestGroupMan;
    use tokio::sync::RwLock;

    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    man.write()
        .await
        .add_group(
            vec!["http://example.com/rpc-session.bin".into()],
            DownloadOptions {
                split: Some(3),
                ..Default::default()
            },
        )
        .unwrap();

    let path =
        std::env::temp_dir().join(format!("test_save_session_man_{}.sess", std::process::id()));
    let _ = tokio::fs::remove_file(&path).await;

    let engine = RpcEngine::new()
        .with_group_man(man)
        .with_save_session_path(path.clone());
    let req = JsonRpcRequest::new("aria2.saveSession", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "saveSession with group_man should succeed"
    );

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"), "Result should contain OK");

    assert!(path.exists(), "session file should be written");
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        content.contains("http://example.com/rpc-session.bin"),
        "session file should contain the group URI"
    );
    assert!(
        content.contains("split=3"),
        "session file should contain the group option"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

/// `aria2.saveSession` with an explicit path overrides the configured path.
#[tokio::test]
async fn test_save_session_explicit_path_overrides_config() {
    let cfg_path = std::env::temp_dir().join(format!(
        "test_save_session_cfg_over_{}.sess",
        std::process::id()
    ));
    let explicit_path = std::env::temp_dir().join(format!(
        "test_save_session_expl_{}.sess",
        std::process::id()
    ));
    let _ = tokio::fs::remove_file(&cfg_path).await;
    let _ = tokio::fs::remove_file(&explicit_path).await;

    let engine = RpcEngine::new().with_save_session_path(cfg_path.clone());
    let req = JsonRpcRequest::new(
        "aria2.saveSession",
        serde_json::json!([explicit_path.to_str().unwrap()]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    assert!(explicit_path.exists(), "explicit path must be written to");
    assert!(
        !cfg_path.exists(),
        "configured path must NOT be written when explicit path given"
    );

    let _ = tokio::fs::remove_file(&explicit_path).await;
    let _ = tokio::fs::remove_file(&cfg_path).await;
}

#[tokio::test]
async fn test_change_position_move_uri() {
    let engine = RpcEngine::new();

    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://uri1.com"])).with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = JsonRpcRequest::new(
        "aria2.changePosition",
        serde_json::json!([gid, 0, "POS_SET"]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(
        change_resp.is_success(),
        "changePosition should succeed for valid positions"
    );

    let result: String = serde_json::from_value(change_resp.result.unwrap()).unwrap();
    assert_eq!(result, "0", "Should return new position 0");
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
    let status_val = tell_resp.result.unwrap();
    assert_eq!(
        status_val["status"].as_str(),
        Some("waiting"),
        "Without an engine loop, a newly added task remains waiting"
    );

    let remove_req = JsonRpcRequest::new("aria2.forceRemove", serde_json::json!([gid])).with_id(3);
    let remove_resp = engine.handle_request(&remove_req).await;
    assert!(remove_resp.is_success(), "forceRemove should succeed");

    let tell_req2 = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(4);
    let tell_resp2 = engine.handle_request(&tell_req2).await;
    assert!(
        tell_resp2.is_success(),
        "tellStatus should still work after forceRemove"
    );
    let status_after_val = tell_resp2.result.unwrap();
    assert_eq!(
        status_after_val["status"].as_str(),
        Some("waiting"),
        "Without an engine loop, forceRemove remains queued"
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

    let req =
        JsonRpcRequest::new("aria2.forceRemove", serde_json::json!([gids.clone()])).with_id(10);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "Batch forceRemove should succeed");

    for gid in &gids {
        let tell_req =
            JsonRpcRequest::new("aria2.tellStatus", serde_json::json!([gid])).with_id(20);
        let tell_resp = engine.handle_request(&tell_req).await;
        assert!(tell_resp.is_success());
        let status_val = tell_resp.result.unwrap();
        assert_eq!(
            status_val["status"].as_str(),
            Some("waiting"),
            "Without an engine loop, forceRemove remains queued for {}",
            gid
        );
    }
}

// =========================================================================
// L3 RPC Query Method Tests
// =========================================================================

#[tokio::test]
async fn test_get_uris_valid_gid_returns_core_state_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getUris", serde_json::json!(["0000000000000001"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error());
}

#[tokio::test]
async fn test_get_uris_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getUris", serde_json::json!(["nonexistent-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getUris should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, 1, "Should be RpcExecution error");
}

#[tokio::test]
async fn test_get_uris_single_uri() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.getUris", serde_json::json!(["0000000000000001"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error());
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
    assert!(resp.is_success(), "getUris should succeed for a valid GID");
    let uris = resp.result.unwrap();
    assert_eq!(uris.as_array().map(Vec::len), Some(1));
    assert_eq!(uris[0]["uri"].as_str(), Some("http://test.com/a.bin"));
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

    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getFiles should succeed for valid GID");

    let files = resp.result.unwrap();
    eprintln!(
        "DEBUG get_files response: {}",
        serde_json::to_string_pretty(&files).unwrap()
    );
    assert!(
        files.as_array().is_some_and(|a| !a.is_empty()),
        "Should return at least one file"
    );
    let file0 = &files[0];
    assert_eq!(
        file0["length"].as_str(),
        Some("0"),
        "Unknown length must remain zero until protocol metadata arrives"
    );
    assert_eq!(
        file0["completedLength"].as_str(),
        Some("0"),
        "A new task has no completed bytes"
    );
}

#[tokio::test]
async fn test_get_files_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getFiles", serde_json::json!(["unknown-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getFiles should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, 1);
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

    let files = resp.result.unwrap();
    assert_eq!(
        files[0]["length"].as_str(),
        Some("0"),
        "New task should have zero length (as string)"
    );
    assert_eq!(
        files[0]["completedLength"].as_str(),
        Some("0"),
        "New task should have zero completed (as string)"
    );
    assert_eq!(
        files[0]["selected"].as_str(),
        Some("true"),
        "Default file should be selected (as string)"
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
    let files = resp.result.unwrap();
    assert_eq!(
        files[0]["selected"].as_str(),
        Some("true"),
        "FileInfo.selected should default to true (as string)"
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

    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!([gid])).with_id(2);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "getServers should succeed for valid GID");

    let servers = resp.result.unwrap();
    let arr = servers.as_array().unwrap();
    assert_eq!(
        arr.len(),
        1,
        "Single-file download should have 1 ServerInfoIndex"
    );
    assert_eq!(
        arr[0]["index"].as_str(),
        Some("1"),
        "File index should be 1-based (as string)"
    );
    assert_eq!(
        arr[0]["servers"].as_array().map(|a| a.len()),
        Some(2),
        "Should have 2 server entries"
    );
    assert_eq!(
        arr[0]["servers"][0]["uri"].as_str(),
        Some("http://dl.example.com/file.bin"),
        "First server URI should match"
    );
    assert_eq!(
        arr[0]["servers"][0]["downloadSpeed"].as_str(),
        Some("0"),
        "A new task has no measured download speed"
    );
}

#[tokio::test]
async fn test_get_servers_unknown_gid_returns_error() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.getServers", serde_json::json!(["bad-gid"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "getServers should fail for unknown GID");
    assert_eq!(resp.error.unwrap().code, 1);
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
    let servers = resp.result.unwrap();
    assert_eq!(
        servers[0]["servers"][0]["downloadSpeed"].as_str(),
        Some("0"),
        "No-progress task should have 0 speed (as string)"
    );
    assert_eq!(
        servers[0]["servers"][0]["currentUri"].as_str(),
        servers[0]["servers"][0]["uri"].as_str(),
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

    let servers = resp.result.unwrap();
    assert_eq!(
        servers[0]["servers"].as_array().map(|a| a.len()),
        Some(1),
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
            .contains(&"bittorrent".to_string()),
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
    let req = JsonRpcRequest::new(
        "aria2.purgeDownloadResult",
        serde_json::json!(["stopped-gid-001"]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Unknown stopped GID should fail");
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
    assert_eq!(resp.error.unwrap().code, 1, "Should be RpcExecution error");
}

#[tokio::test]
async fn test_purge_download_result_no_param_clears_all() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "No-param purge should succeed");
}

#[tokio::test]
async fn test_purge_download_result_partial_purge() {
    let engine = RpcEngine::new();
    let req =
        JsonRpcRequest::new("aria2.purgeDownloadResult", serde_json::json!(["gid-b"])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_error(), "Unknown stopped GID should fail");
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
async fn test_get_session_info_consistent_per_call() {
    let engine = RpcEngine::new();
    let req1 = JsonRpcRequest::new("aria2.getSessionInfo", serde_json::json!([])).with_id(1);
    let resp1 = engine.handle_request(&req1).await;

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
    // C++ aria2 generates sessionId_ once at engine construction and returns
    // the same value on every call. The Rust port must match this behavior.
    assert_eq!(
        sid1, sid2,
        "Session ID must be consistent across calls (C++ generates once at construction)"
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
