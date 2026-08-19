//! Contract tests for shutdown requests.

mod common;

use aria2_rpc::json_rpc::JsonRpcRequest;
use common::test_engine;
use serde_json::json;

fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

#[tokio::test]
async fn shutdown_reports_the_current_task_count() {
    let engine = test_engine();
    let add = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.zip"]]),
        ))
        .await;
    assert!(add.is_success());
    let shutdown = engine
        .handle_request(&request("aria2.shutdown", json!([])))
        .await;
    assert!(shutdown.result.unwrap().as_str().unwrap().contains("OK"));
    assert_eq!(engine.task_count().await, 1);
}

#[tokio::test]
async fn force_shutdown_clears_backend_tasks() {
    let engine = test_engine();
    for index in 0..3 {
        let response = engine
            .handle_request(&request(
                "aria2.addUri",
                json!([[format!("http://example.test/{index}")]]),
            ))
            .await;
        assert!(response.is_success());
    }
    let response = engine
        .handle_request(&request("aria2.forceShutdown", json!([])))
        .await;
    assert!(response.is_success());
    assert!(
        response
            .result
            .unwrap()
            .as_str()
            .unwrap()
            .contains("3 downloads")
    );
    assert_eq!(engine.task_count().await, 0);
}
