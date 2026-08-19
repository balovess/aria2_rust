//! Contract tests for the pure RPC seam.

mod common;

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::server::{AuthConfig, ServerConfig};
use common::test_engine;
use serde_json::json;

fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

#[tokio::test]
async fn add_uri_returns_gid_and_publishes_start() {
    let engine = test_engine();
    let mut events = engine.publisher().subscribe("add-uri", None).await;

    let response = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.bin"], {"dir": "/tmp"}]),
        ))
        .await;
    assert!(response.is_success());
    let gid = response.result.unwrap().as_str().unwrap().to_string();
    assert_eq!(gid.len(), 16);

    let (_, event) = events.recv().await.unwrap();
    assert_eq!(event.gid(), gid);
}

#[tokio::test]
async fn lifecycle_calls_cross_only_the_backend_interface() {
    let engine = test_engine();
    let add = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.bin"]]),
        ))
        .await;
    let gid = add.result.unwrap().as_str().unwrap().to_owned();

    for method in ["aria2.pause", "aria2.unpause", "aria2.forcePause"] {
        let response = engine
            .handle_request(&request(method, json!([gid.clone()])))
            .await;
        assert!(
            response.is_success(),
            "{method} should succeed: {response:?}"
        );
    }

    let status = engine
        .handle_request(&request("aria2.tellStatus", json!([gid])))
        .await;
    assert_eq!(status.result.unwrap()["status"], "paused");
}

#[tokio::test]
async fn status_projection_is_owned_by_rpc_layer() {
    let engine = test_engine();
    let add = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.bin"]]),
        ))
        .await;
    let gid = add.result.unwrap().as_str().unwrap().to_owned();

    let response = engine
        .handle_request(&request(
            "aria2.tellStatus",
            json!([gid, ["gid", "status"]]),
        ))
        .await;
    let status = response.result.unwrap();
    assert!(status.get("gid").is_some());
    assert!(status.get("status").is_some());
    assert!(status.get("files").is_none());
}

#[tokio::test]
async fn options_are_normalized_and_invalid_values_are_rejected() {
    let engine = test_engine();
    let add = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.bin"]]),
        ))
        .await;
    let gid = add.result.unwrap().as_str().unwrap().to_owned();

    let change = engine
        .handle_request(&request(
            "aria2.changeOption",
            json!([gid.clone(), {"max-download-limit": "2M"}]),
        ))
        .await;
    assert!(change.is_success());
    let options = engine
        .handle_request(&request("aria2.getOption", json!([gid.clone()])))
        .await;
    assert_eq!(options.result.unwrap()["max-download-limit"], "2M");

    let invalid = engine
        .handle_request(&request(
            "aria2.changeOption",
            json!([gid, {"max-download-limit": "not-a-rate"}]),
        ))
        .await;
    assert!(invalid.is_error());
}

#[tokio::test]
async fn read_multicall_uses_one_backend_snapshot() {
    let engine = test_engine();
    for index in 0..2 {
        let response = engine
            .handle_request(&request(
                "aria2.addUri",
                json!([[format!("http://example.test/{index}")]]),
            ))
            .await;
        assert!(response.is_success());
    }

    let response = engine
        .handle_request(&request(
            "system.multicall",
            json!([[
                {"methodName": "aria2.tellActive", "params": []},
                {"methodName": "aria2.tellWaiting", "params": [0, 10]},
                {"methodName": "aria2.getGlobalStat", "params": []}
            ]]),
        ))
        .await;
    assert!(response.is_success());
    let results = response.result.unwrap().as_array().unwrap().to_vec();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|result| result.as_array().is_some()));
}

#[tokio::test]
async fn queue_position_and_uri_changes_return_wire_contracts() {
    let engine = test_engine();
    let add = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/one"]]),
        ))
        .await;
    let gid = add.result.unwrap().as_str().unwrap().to_owned();

    let position = engine
        .handle_request(&request(
            "aria2.changePosition",
            json!([gid.clone(), 0, "POS_SET"]),
        ))
        .await;
    assert!(position.is_success());

    let uris = engine
        .handle_request(&request(
            "aria2.changeUri",
            json!([gid.clone(), 1, [], ["http://example.test/two"]]),
        ))
        .await;
    assert_eq!(uris.result.unwrap(), json!(["0", "1"]));

    let listed = engine
        .handle_request(&request("aria2.getUris", json!([gid])))
        .await;
    assert_eq!(listed.result.unwrap().as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn session_and_shutdown_are_backend_operations() {
    let engine = test_engine();
    let session = engine
        .handle_request(&request("aria2.getSessionInfo", json!([])))
        .await;
    assert!(session.result.unwrap()["sessionId"].is_string());

    let shutdown = engine
        .handle_request(&request("aria2.shutdown", json!([])))
        .await;
    assert!(shutdown.result.unwrap().as_str().unwrap().contains("OK"));
}

#[tokio::test]
async fn standalone_engine_is_explicitly_backendless() {
    let engine = RpcEngine::new();
    let response = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.bin"]]),
        ))
        .await;
    assert!(response.is_error());
}

#[test]
fn server_configuration_and_auth_remain_transport_only() {
    let config = ServerConfig::default();
    assert_eq!(config.port, 6800);
    let auth = AuthConfig::default().with_token("secret");
    assert!(auth.verify_token("secret"));
    assert!(!auth.verify_token("wrong"));
}
