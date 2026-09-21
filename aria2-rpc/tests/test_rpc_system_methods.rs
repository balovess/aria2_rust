//! Integration tests for system.listMethods and system.listNotifications.
//!
//! Tests the system discovery methods from aria2 RPC specification.

mod common;

use aria2_rpc::json_rpc::JsonRpcRequest;
use common::test_engine;

#[allow(unused_mut)]
fn expected_methods() -> Vec<String> {
    let mut methods = vec![
        "aria2.addUri",
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
        "aria2.updateBrowserContext",
        "aria2.clearBrowserContext",
        "system.multicall",
        "system.listMethods",
        "system.listNotifications",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    #[cfg(feature = "bittorrent")]
    methods.splice(
        1..1,
        [
            "aria2.addTorrent",
            "aria2.getPeers",
            "aria2.getTrackers",
            "aria2.getDhtStatus",
            "aria2.saveDhtState",
            "aria2.evictDhtNodes",
        ]
        .into_iter()
        .map(str::to_string),
    );

    #[cfg(feature = "metalink")]
    {
        let index = methods
            .iter()
            .position(|method| method == "aria2.remove")
            .expect("base method catalog must contain aria2.remove");
        methods.insert(index, "aria2.addMetalink".to_string());
    }

    methods
}

#[allow(unused_mut)]
fn expected_notifications() -> Vec<String> {
    let mut notifications = vec![
        "aria2.onDownloadStart",
        "aria2.onDownloadPause",
        "aria2.onDownloadStop",
        "aria2.onDownloadComplete",
        "aria2.onDownloadError",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    #[cfg(feature = "bittorrent")]
    notifications.push("aria2.onBtDownloadComplete".to_string());

    notifications
}

#[tokio::test]
async fn test_list_methods_returns_all_methods() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(methods, expected_methods());
}

#[tokio::test]
async fn test_list_methods_contains_core_methods() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Core task management methods
    assert!(methods.contains(&"aria2.addUri".to_string()));
    assert!(methods.contains(&"aria2.remove".to_string()));
    assert!(methods.contains(&"aria2.forceRemove".to_string()));
    assert!(methods.contains(&"aria2.pause".to_string()));
    assert!(methods.contains(&"aria2.unpause".to_string()));
}

#[tokio::test]
async fn test_list_methods_contains_shutdown_methods() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Shutdown methods
    assert!(methods.contains(&"aria2.shutdown".to_string()));
    assert!(methods.contains(&"aria2.forceShutdown".to_string()));
}

#[tokio::test]
async fn test_list_methods_contains_system_methods() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // System methods
    assert!(methods.contains(&"system.multicall".to_string()));
    assert!(methods.contains(&"system.listMethods".to_string()));
    assert!(methods.contains(&"system.listNotifications".to_string()));
}

#[tokio::test]
async fn test_list_notifications_returns_all_events() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(notifications, expected_notifications());
}

#[tokio::test]
async fn test_list_notifications_contains_core_events() {
    let engine = test_engine();
    let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Core download events
    assert!(notifications.contains(&"aria2.onDownloadStart".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadPause".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadStop".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadComplete".to_string()));
    assert!(notifications.contains(&"aria2.onDownloadError".to_string()));
}

#[tokio::test]
async fn test_rpc_coverage_100_percent() {
    // Verify that all methods listed by listMethods are actually callable
    let engine = test_engine();

    let list_req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let list_resp = engine.handle_request(&list_req).await;
    let methods: Vec<String> = serde_json::from_value(list_resp.result.unwrap()).unwrap();

    // Test that each method is routable (no "Method not found" error)
    for method in &methods {
        let test_req = JsonRpcRequest::new(method, serde_json::json!([])).with_id(1);
        let test_resp = engine.handle_request(&test_req).await;

        // Should not return "Method not found" (-32601)
        if test_resp.is_error() {
            let error = test_resp.error.unwrap();
            // Only allow parameter errors, not method not found
            assert_ne!(error.code, -32601, "Method {} should be routable", method);
        }
    }
}
