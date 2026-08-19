//! RPC/backend contract tests for task options and status data.

mod common;

use aria2_rpc::json_rpc::JsonRpcRequest;
use common::test_engine;
use serde_json::json;

fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

#[tokio::test]
async fn add_uri_preserves_array_options_at_the_seam() {
    let engine = test_engine();
    let response = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([[
                "http://example.test/file.zip"
            ], {
                "header": ["Referer: https://example.test", "User-Agent: TestAgent/1.0"],
                "user-agent": "MyCustomUA",
                "referer": "https://referer.example",
                "dir": "/tmp/downloads",
                "out": "file.zip",
                "split": 8
            }]),
        ))
        .await;
    assert!(response.is_success());
    let gid = response.result.unwrap().as_str().unwrap().to_owned();

    let options = engine
        .handle_request(&request("aria2.getOption", json!([gid])))
        .await;
    let options = options.result.unwrap();
    assert_eq!(
        options["header"],
        "Referer: https://example.test\nUser-Agent: TestAgent/1.0"
    );
    assert_eq!(options["user-agent"], "MyCustomUA");
    assert_eq!(options["dir"], "/tmp/downloads");
}

#[tokio::test]
async fn add_uri_preserves_string_headers_without_core_types() {
    let engine = test_engine();
    let response = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/file.zip"], {
                "header": "Referer: https://example.test\nUser-Agent: TestAgent/1.0\n"
            }]),
        ))
        .await;
    assert!(response.is_success());
    let gid = response.result.unwrap().as_str().unwrap().to_owned();
    let options = engine
        .handle_request(&request("aria2.getOption", json!([gid])))
        .await;
    assert_eq!(
        options.result.unwrap()["header"],
        "Referer: https://example.test\nUser-Agent: TestAgent/1.0\n"
    );
}

#[tokio::test]
async fn tell_status_returns_the_backend_snapshot_shape() {
    let engine = test_engine();
    let response = engine
        .handle_request(&request(
            "aria2.addUri",
            json!([["http://example.test/largefile.bin"]]),
        ))
        .await;
    let gid = response.result.unwrap().as_str().unwrap().to_owned();

    let status = engine
        .handle_request(&request("aria2.tellStatus", json!([gid])))
        .await;
    let status = status.result.unwrap();
    assert_eq!(status["status"], "active");
    assert_eq!(status["totalLength"], "0");
    assert_eq!(status["completedLength"], "0");
    assert_eq!(status["downloadSpeed"], "0");
    assert!(status["files"].is_array());
}

#[tokio::test]
async fn global_stat_and_active_queries_share_consistent_counts() {
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

    let active = engine
        .handle_request(&request("aria2.tellActive", json!([])))
        .await;
    assert_eq!(active.result.unwrap().as_array().unwrap().len(), 2);
    let stat = engine
        .handle_request(&request("aria2.getGlobalStat", json!([])))
        .await;
    assert_eq!(stat.result.unwrap()["numActive"], "2");
}

#[tokio::test]
async fn task_option_changes_are_visible_on_subsequent_reads() {
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
            json!([gid.clone(), {"max-upload-limit": "512K"}]),
        ))
        .await;
    assert!(change.is_success());
    let options = engine
        .handle_request(&request("aria2.getOption", json!([gid])))
        .await;
    assert_eq!(options.result.unwrap()["max-upload-limit"], "512K");
}
