//! Integration tests for RPC shutdown methods.
//!
//! Tests `aria2.shutdown` and `aria2.forceShutdown` functionality.

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;

#[tokio::test]
async fn test_shutdown_empty_engine() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.shutdown", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("0 active downloads"));
}

#[tokio::test]
async fn test_shutdown_with_active_downloads() {
    let engine = RpcEngine::new();

    // Add a download task
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://example.com/file.zip"]))
            .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    assert!(add_resp.is_success());

    // Call shutdown
    let shutdown_req = JsonRpcRequest::new("aria2.shutdown", serde_json::json!([])).with_id(2);
    let shutdown_resp = engine.handle_request(&shutdown_req).await;
    assert!(shutdown_resp.is_success());

    let result: String = serde_json::from_value(shutdown_resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("1 active downloads"));
}

#[tokio::test]
async fn test_force_shutdown_empty_engine() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest::new("aria2.forceShutdown", serde_json::json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());

    let result: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("0 downloads"));
}

#[tokio::test]
async fn test_force_shutdown_with_active_downloads() {
    let engine = RpcEngine::new();

    // Add multiple download tasks
    for i in 0..3 {
        let add_req = JsonRpcRequest::new(
            "aria2.addUri",
            serde_json::json!([format!("http://example.com/file{}.zip", i)]),
        )
        .with_id(i);
        let add_resp = engine.handle_request(&add_req).await;
        assert!(add_resp.is_success());
    }

    // Verify tasks exist
    assert_eq!(engine.task_count().await, 3);

    // Call forceShutdown
    let force_shutdown_req =
        JsonRpcRequest::new("aria2.forceShutdown", serde_json::json!([])).with_id(100);
    let force_shutdown_resp = engine.handle_request(&force_shutdown_req).await;
    assert!(force_shutdown_resp.is_success());

    let result: String = serde_json::from_value(force_shutdown_resp.result.unwrap()).unwrap();
    assert!(result.contains("OK"));
    assert!(result.contains("3 downloads forcibly terminated"));

    // Verify all tasks cleared
    assert_eq!(engine.task_count().await, 0);
}

#[tokio::test]
async fn test_shutdown_vs_force_shutdown_difference() {
    let engine = RpcEngine::new();

    // Add a task
    let add_req =
        JsonRpcRequest::new("aria2.addUri", serde_json::json!(["http://example.com/test.zip"]))
            .with_id(1);
    engine.handle_request(&add_req).await;

    // shutdown should keep tasks (graceful)
    let shutdown_req = JsonRpcRequest::new("aria2.shutdown", serde_json::json!([])).with_id(2);
    let shutdown_resp = engine.handle_request(&shutdown_req).await;
    assert!(shutdown_resp.is_success());
    assert_eq!(engine.task_count().await, 1); // Tasks still exist

    // forceShutdown should clear tasks (immediate)
    let force_shutdown_req =
        JsonRpcRequest::new("aria2.forceShutdown", serde_json::json!([])).with_id(3);
    let force_shutdown_resp = engine.handle_request(&force_shutdown_req).await;
    assert!(force_shutdown_resp.is_success());
    assert_eq!(engine.task_count().await, 0); // Tasks cleared
}