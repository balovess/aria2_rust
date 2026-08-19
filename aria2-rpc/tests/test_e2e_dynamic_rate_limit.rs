//! Dynamic option contract tests.
//!
//! Rate limiter application is an `aria2` backend concern. This crate only
//! verifies that the RPC layer parses and forwards the option values without
//! depending on application-core types.

mod common;

use aria2_rpc::json_rpc::JsonRpcRequest;
use common::test_engine;
use serde_json::json;

fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest::new(method, params).with_id(1)
}

#[tokio::test]
async fn global_rate_options_round_trip_as_rpc_strings() {
    let engine = test_engine();
    let response = engine
        .handle_request(&request(
            "aria2.changeGlobalOption",
            json!([{
                "max-overall-download-limit": "2M",
                "max-overall-upload-limit": "512K"
            }]),
        ))
        .await;
    assert!(response.is_success());

    let options = engine
        .handle_request(&request("aria2.getGlobalOption", json!([])))
        .await;
    let options = options.result.unwrap();
    assert_eq!(options["max-overall-download-limit"], "2M");
    assert_eq!(options["max-overall-upload-limit"], "512K");
}

#[tokio::test]
async fn invalid_rate_options_are_rejected_before_backend_success() {
    let engine = test_engine();
    let response = engine
        .handle_request(&request(
            "aria2.changeGlobalOption",
            json!([{"max-overall-download-limit": "not-a-rate"}]),
        ))
        .await;
    assert!(response.is_error());
    assert_eq!(response.error.unwrap().code, 1);
}
