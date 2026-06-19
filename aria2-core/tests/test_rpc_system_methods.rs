//! Integration tests for system.listMethods and system.listNotifications.
//!
//! Tests the system discovery methods from aria2 RPC specification.

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;

#[tokio::test]
async fn test_list_methods_returns_all_methods() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(methods.len(), 36);
}

#[tokio::test]
async fn test_list_methods_contains_core_methods() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Core task management methods
    assert!(methods.contains(&"aria2.addUri".to_string()));
    assert!(methods.contains(&"aria2.addTorrent".to_string()));
    assert!(methods.contains(&"aria2.addMetalink".to_string()));
    assert!(methods.contains(&"aria2.remove".to_string()));
    assert!(methods.contains(&"aria2.forceRemove".to_string()));
    assert!(methods.contains(&"aria2.pause".to_string()));
    assert!(methods.contains(&"aria2.unpause".to_string()));
}

#[tokio::test]
async fn test_list_methods_contains_shutdown_methods() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("system.listMethods", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let methods: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Shutdown methods
    assert!(methods.contains(&"aria2.shutdown".to_string()));
    assert!(methods.contains(&"aria2.forceShutdown".to_string()));
}

#[tokio::test]
async fn test_list_methods_contains_system_methods() {
    let engine = RpcEngine::new();
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
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(notifications.len(), 7);
}

#[tokio::test]
async fn test_list_notifications_contains_core_events() {
    let engine = RpcEngine::new();
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
async fn test_list_notifications_contains_bt_events() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("system.listNotifications", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;

    let notifications: Vec<String> = serde_json::from_value(resp.result.unwrap()).unwrap();

    // BitTorrent-specific events
    assert!(notifications.contains(&"aria2.onBtDownloadComplete".to_string()));
    assert!(notifications.contains(&"aria2.onBtDownloadError".to_string()));
}

#[tokio::test]
async fn test_rpc_coverage_100_percent() {
    // Verify that all methods listed by listMethods are actually callable
    let engine = RpcEngine::new();

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