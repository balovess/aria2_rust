//! Fixed wire-contract tests for the public aria2 RPC boundary.
//!
//! These tests intentionally assert literal protocol semantics rather than
//! merely checking that a method is routable. The in-memory backend keeps the
//! cases deterministic while exercising the same public `RpcEngine` seam used
//! by HTTP and WebSocket transports.

mod common;

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::{
    JsonRpcError, JsonRpcRequest, JsonRpcWireEntry, parse_aria2_wire_document,
};
use aria2_rpc::websocket::{DownloadEvent, EventType};
use aria2_rpc::xml_rpc::{XmlRpcError, XmlRpcMember, XmlRpcRequest, XmlRpcResponse, XmlRpcValue};
use common::test_engine;
use serde_json::{Value, json};

async fn call(engine: &RpcEngine, method: &str, params: Value, id: &str) -> Value {
    let request = JsonRpcRequest::new(method, params).with_id(id);
    serde_json::to_value(engine.handle_request(&request).await).expect("response is serializable")
}

fn result<'a>(response: &'a Value, method: &str) -> &'a Value {
    assert_eq!(response["jsonrpc"], "2.0", "{method} must use JSON-RPC 2.0");
    assert!(
        response.get("error").is_none(),
        "{method} failed: {response}"
    );
    response
        .get("result")
        .unwrap_or_else(|| panic!("{method} omitted result: {response}"))
}

fn error_code(response: &Value, method: &str) -> i64 {
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response.get("result").is_none(), "{method} returned result");
    response["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("{method} has no numeric error code: {response}"))
}

#[test]
fn json_wire_parser_locks_aria2_envelope_rules() {
    let omitted_members =
        parse_aria2_wire_document(br#"{"id":"request-1","method":"aria2.getVersion"}"#)
            .expect("valid request");
    assert!(!omitted_members.is_batch);
    match &omitted_members.entries[0] {
        JsonRpcWireEntry::Request(request) => {
            assert_eq!(request.version, None);
            assert_eq!(request.params, json!([]));
            assert_eq!(request.id, Some(json!("request-1")));
        }
        JsonRpcWireEntry::Error(response) => panic!("unexpected error: {response:?}"),
    }

    let batch = parse_aria2_wire_document(
        br#"[{"jsonrpc":"1.0","id":1,"method":"aria2.getVersion"},42,{"method":"aria2.getVersion"}]"#,
    )
    .expect("valid aria2 batch");
    assert!(batch.is_batch);
    assert_eq!(
        batch.entries.len(),
        2,
        "non-object batch values are ignored"
    );
    assert!(matches!(batch.entries[0], JsonRpcWireEntry::Request(_)));
    match &batch.entries[1] {
        JsonRpcWireEntry::Error(response) => {
            assert_eq!(response.id, Value::Null);
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(-32600)
            );
            assert_eq!(
                response.error.as_ref().map(|error| error.message.as_str()),
                Some("Invalid Request.")
            );
        }
        JsonRpcWireEntry::Request(request) => panic!("unexpected request: {request:?}"),
    }

    let empty_batch = parse_aria2_wire_document(b"[]").expect("empty batch is valid aria2 wire");
    assert!(empty_batch.is_batch);
    assert!(empty_batch.entries.is_empty());

    let parse_error = parse_aria2_wire_document(br#"{"#).expect_err("malformed JSON must fail");
    assert_eq!(parse_error.code(), -32700);
    let invalid_root = parse_aria2_wire_document(br#"null"#).expect_err("null is not a request");
    assert_eq!(invalid_root.code(), -32600);
}

#[tokio::test]
async fn json_error_contract_is_fixed() {
    let engine = test_engine();

    let unknown = call(&engine, "aria2.forceUnpause", json!([]), "unknown").await;
    assert_eq!(unknown["id"], "unknown");
    assert_eq!(error_code(&unknown, "unknown method"), 1);
    assert_eq!(
        unknown["error"]["message"],
        "No such method: aria2.forceUnpause"
    );

    let invalid_params = call(&engine, "aria2.addUri", json!([]), "invalid").await;
    assert_eq!(error_code(&invalid_params, "invalid params"), -32602);
    assert_eq!(invalid_params["error"]["message"], "param[0] not found");

    let recursive = call(
        &engine,
        "system.multicall",
        json!([[{"methodName":"system.multicall","params":[]}]]),
        "recursive",
    )
    .await;
    assert!(recursive.get("error").is_none());
    assert_eq!(
        recursive["result"],
        json!([{
            "code": 1,
            "message": "Recursive system.multicall forbidden."
        }])
    );
}

#[tokio::test]
async fn standard_method_results_keep_aria2_wire_types() {
    let engine = test_engine();

    let add = call(
        &engine,
        "aria2.addUri",
        json!([["https://example.test/file.bin"], {"pause":"true"}]),
        "add",
    )
    .await;
    let gid = result(&add, "addUri")
        .as_str()
        .expect("addUri returns a string GID")
        .to_string();
    assert_eq!(gid.len(), 16);
    assert!(gid.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let status = call(&engine, "aria2.tellStatus", json!([gid]), "status").await;
    let status = result(&status, "tellStatus");
    let expected_keys = [
        "gid",
        "totalLength",
        "completedLength",
        "uploadLength",
        "downloadSpeed",
        "uploadSpeed",
        "connections",
        "status",
        "dir",
        "files",
    ];
    let mut actual_keys = status
        .as_object()
        .expect("tellStatus returns an object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    actual_keys.sort_unstable();
    let mut expected_keys = expected_keys.to_vec();
    expected_keys.sort_unstable();
    assert_eq!(actual_keys, expected_keys);
    for key in [
        "totalLength",
        "completedLength",
        "uploadLength",
        "downloadSpeed",
        "uploadSpeed",
        "connections",
    ] {
        assert!(status[key].is_string(), "{key} must be a wire string");
    }
    assert_eq!(status["status"], "active");
    assert_eq!(status["files"][0]["index"], "1");
    assert_eq!(status["files"][0]["length"], "0");
    assert_eq!(status["files"][0]["completedLength"], "0");
    assert_eq!(status["files"][0]["selected"], "true");
    assert_eq!(status["files"][0]["uris"][0]["status"], "waiting");

    let filtered = call(
        &engine,
        "aria2.tellStatus",
        json!([gid, ["gid", "completedLength", "files", "unknownField"]]),
        "filtered",
    )
    .await;
    assert_eq!(
        result(&filtered, "filtered"),
        &json!({"gid": gid, "completedLength": "0", "files": status["files"]})
    );

    let global_stat = call(&engine, "aria2.getGlobalStat", json!([]), "stat").await;
    let global_stat = result(&global_stat, "getGlobalStat");
    for key in [
        "downloadSpeed",
        "uploadSpeed",
        "numActive",
        "numWaiting",
        "numStopped",
        "numStoppedTotal",
    ] {
        assert!(global_stat[key].is_string(), "{key} must be a wire string");
    }

    let change_uri = call(
        &engine,
        "aria2.changeUri",
        json!([
            gid,
            1,
            ["https://example.test/file.bin"],
            ["https://mirror.test/file.bin"]
        ]),
        "uri",
    )
    .await;
    assert_eq!(result(&change_uri, "changeUri"), &json!(["1", "1"]));

    let change_position = call(
        &engine,
        "aria2.changePosition",
        json!([gid, 0, "POS_SET"]),
        "position",
    )
    .await;
    assert_eq!(result(&change_position, "changePosition"), &json!(0));

    let pause = call(&engine, "aria2.pause", json!([gid]), "pause").await;
    assert_eq!(result(&pause, "pause"), &json!(gid));
    let unpause = call(&engine, "aria2.unpause", json!([gid]), "unpause").await;
    assert_eq!(result(&unpause, "unpause"), &json!(gid));
    let force_pause = call(&engine, "aria2.forcePause", json!([gid]), "force-pause").await;
    assert_eq!(result(&force_pause, "forcePause"), &json!(gid));

    for method in [
        "aria2.changeOption",
        "aria2.changeGlobalOption",
        "aria2.pauseAll",
        "aria2.forcePauseAll",
        "aria2.unpauseAll",
        "aria2.updateBrowserContext",
        "aria2.clearBrowserContext",
    ] {
        let params = match method {
            "aria2.pause" | "aria2.forcePause" | "aria2.unpause" => json!([gid]),
            "aria2.changeOption" => json!([gid, {"max-download-limit":"1K"}]),
            "aria2.changeGlobalOption" => json!([{"max-overall-download-limit":"1K"}]),
            "aria2.updateBrowserContext" => {
                json!([{"cookie":"a=b","userAgent":"test","headers":[]}])
            }
            _ => json!([]),
        };
        let response = call(&engine, method, params, method).await;
        assert!(
            result(&response, method).is_string(),
            "{method} returns text"
        );
    }
}

#[test]
fn xml_rpc_contract_keeps_aria2_envelopes_and_fault_codes() {
    let request = XmlRpcRequest::new("aria2.getVersion", vec![XmlRpcValue::string("secret")]);
    let request_xml = request.to_xml();
    assert!(request_xml.starts_with("<?xml version=\"1.0\"?>"));
    assert!(request_xml.contains("<methodName>aria2.getVersion</methodName>"));
    assert!(request_xml.contains("<string>secret</string>"));

    let response = XmlRpcResponse::single(XmlRpcValue::struct_(vec![XmlRpcMember::new(
        "version",
        XmlRpcValue::string("0.3.8"),
    )]));
    let response_xml = response.to_xml();
    assert!(response_xml.contains("<methodResponse>"));
    assert!(response_xml.contains("<name>version</name>"));
    assert!(response_xml.contains("<string>0.3.8</string>"));

    let fault = XmlRpcResponse::fault(1, "No such method: aria2.forceUnpause");
    let fault_xml = fault.to_xml();
    assert!(fault_xml.contains("<name>faultCode</name>"));
    assert!(fault_xml.contains("<int>1</int>"));
    assert!(fault_xml.contains("No such method: aria2.forceUnpause"));

    assert_eq!(XmlRpcError::RpcExecution("x".into()).fault_code(), 1);
    assert_eq!(XmlRpcError::InvalidParams("x".into()).fault_code(), -32602);
}

#[test]
fn websocket_notification_contract_is_fixed() {
    let event = DownloadEvent::download_complete("0123456789abcdef");
    assert_eq!(event.event_type(), Some(EventType::DownloadComplete));
    assert_eq!(event.method(), "aria2.onDownloadComplete");
    assert_eq!(event.gid(), "0123456789abcdef");
    assert_eq!(
        serde_json::from_str::<Value>(&event.to_json().unwrap()).unwrap(),
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.onDownloadComplete",
            "params": [{"gid": "0123456789abcdef"}]
        })
    );
    assert!(EventType::from_method("aria2.onUnknown").is_none());
}

#[test]
fn json_error_enum_codes_match_json_rpc_and_aria2() {
    assert_eq!(JsonRpcError::ParseError("x".into()).code(), -32700);
    assert_eq!(JsonRpcError::InvalidRequest("x".into()).code(), -32600);
    assert_eq!(JsonRpcError::MethodNotFound("x".into()).code(), -32601);
    assert_eq!(JsonRpcError::InvalidParams("x".into()).code(), -32602);
    assert_eq!(JsonRpcError::InternalError("x".into()).code(), -32603);
    assert_eq!(JsonRpcError::RpcExecution("x".into()).code(), 1);
    assert_eq!(
        JsonRpcError::Unauthorized("details".into()).message(),
        "Unauthorized"
    );
}
