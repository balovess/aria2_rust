use super::*;

#[test]
fn test_parse_valid_request() {
    let raw = r#"{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":["http://example.com/file.iso"]}"#;
    let requests = parse_request(raw.as_bytes()).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "aria2.addUri");
    assert!(!requests[0].is_notification());
}

#[test]
fn test_notification_no_id() {
    let raw = r#"{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[{"gid":"abc123"}]}"#;
    let requests = parse_request(raw.as_bytes()).unwrap();
    assert!(requests[0].is_notification());
}

#[test]
fn test_batch_requests() {
    let raw = r#"[{"jsonrpc":"2.0","id":1,"method":"aria2.addUri","params":["url1"]},{"jsonrpc":"2.0","id":2,"method":"aria2.addUri","params":["url2"]}]"#;
    let requests = parse_request(raw.as_bytes()).unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn test_batch_empty_rejects() {
    let raw = r#"[]"#;
    assert!(parse_request(raw.as_bytes()).is_err());
}

#[test]
fn test_aria2_wire_parser_ignores_version_and_defaults_params() {
    let raw = br#"{"jsonrpc":"1.0","id":1,"method":"aria2.getVersion"}"#;
    let document = parse_aria2_wire_document(raw).unwrap();
    assert!(!document.is_batch);

    let JsonRpcWireEntry::Request(request) = &document.entries[0] else {
        panic!("expected a request entry");
    };
    assert_eq!(request.version.as_deref(), Some("1.0"));
    assert_eq!(request.params, serde_json::json!([]));
}

#[test]
fn test_aria2_wire_parser_materializes_object_errors() {
    let raw = br#"[{"id":1,"method":"aria2.getVersion","params":{}},{"method":"aria2.getVersion"},42,"ignored",[1,2],true,null]"#;
    let document = parse_aria2_wire_document(raw).unwrap();
    assert!(document.is_batch);
    assert_eq!(
        document.entries.len(),
        2,
        "non-object batch items are ignored"
    );

    let JsonRpcWireEntry::Error(error) = &document.entries[0] else {
        panic!("object params must be rejected before dispatch");
    };
    assert_eq!(error.error.as_ref().map(|error| error.code), Some(-32602));
    assert_eq!(error.id, serde_json::json!(1));

    let JsonRpcWireEntry::Error(error) = &document.entries[1] else {
        panic!("missing id must be rejected before dispatch");
    };
    assert_eq!(error.error.as_ref().map(|error| error.code), Some(-32600));
    assert!(error.id.is_null());
}

#[test]
fn test_aria2_wire_parser_preserves_empty_batch() {
    let document = parse_aria2_wire_document(b"[]").unwrap();
    assert!(document.is_batch);
    assert!(document.entries.is_empty());
}

#[test]
fn test_invalid_json() {
    let raw = b"{broken";
    let err = parse_request(raw).unwrap_err();
    assert_eq!(err.code(), -32700);
}

#[test]
fn test_invalid_method() {
    let raw = r#"{"jsonrpc":"2.0","id":1,"method":""}"#;
    let err = parse_request(raw.as_bytes()).unwrap_err();
    assert_eq!(err.code(), -32600);
}

#[test]
fn test_valid_json_scalar_is_invalid_request() {
    let err = parse_request(b"42").unwrap_err();
    assert_eq!(err.code(), -32600);
}

#[test]
fn test_get_param_positional() {
    let req = JsonRpcRequest::new("test", serde_json::json!(["hello", 42]));
    let s: String = req.get_param(0).unwrap();
    let n: i64 = req.get_param(1).unwrap();
    assert_eq!(s, "hello");
    assert_eq!(n, 42);
}

#[test]
fn test_get_param_named() {
    let req = JsonRpcRequest::new(
        "test",
        serde_json::json!({"uri": "http://example.com", "dir": "/tmp"}),
    );
    let uri: String = req.get_param_by_name("uri").unwrap();
    let dir: String = req.get_param_by_name("dir").unwrap();
    assert_eq!(uri, "http://example.com");
    assert_eq!(dir, "/tmp");
}

#[test]
fn test_get_param_missing() {
    let req = JsonRpcRequest::new("test", serde_json::json!(["only_one"]));
    assert!(req.get_param::<String>(1).is_err());
}

#[test]
fn test_take_param_moves_positional_value() {
    let mut req = JsonRpcRequest::new(
        "test",
        serde_json::json!([["https://example.com/a", "https://example.com/b"], {"dir": "/tmp"}]),
    );

    let uris: Vec<String> = req.take_param(0).unwrap();

    assert_eq!(uris.len(), 2);
    assert!(req.params[0].is_null());
    assert_eq!(req.params[1]["dir"], "/tmp");
}

#[test]
fn test_take_optional_param_preserves_missing_and_named_semantics() {
    let mut req = JsonRpcRequest::new("test", serde_json::json!({"p1": {"dir": "/tmp"}}));

    let missing: Option<String> = req.take_optional_param(0).unwrap();
    let options: Option<std::collections::HashMap<String, serde_json::Value>> =
        req.take_optional_param(1).unwrap();

    assert_eq!(missing, None);
    assert_eq!(options.unwrap()["dir"], "/tmp");
    assert!(req.params.as_object().unwrap().is_empty());
}

#[test]
fn test_take_param_reports_type_error_like_borrowed_accessor() {
    let mut req = JsonRpcRequest::new("test", serde_json::json!([42]));

    let error = req.take_param::<String>(0).unwrap_err();

    assert_eq!(error.code(), -32602);
    assert!(error.to_string().contains("param[0] type error"));
}

#[test]
fn test_response_success() {
    let resp = JsonRpcResponse::success(1, "2089de05e05901bc1d7d8e048d8d716");
    assert!(resp.is_success());
    assert!(!resp.is_error());
    assert_eq!(resp.result.unwrap(), "2089de05e05901bc1d7d8e048d8d716");
}

#[test]
fn test_response_error() {
    let resp = JsonRpcResponse::error(1, -32601, "Method not found");
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[test]
fn test_response_serialize() {
    let resp = JsonRpcResponse::success(42, serde_json::json!({"gid": "abc"}));
    let json = resp.to_string().unwrap();
    assert!(json.contains("\"id\":42"));
    assert!(json.contains("\"result\""));
}

#[test]
fn test_error_codes() {
    assert_eq!(JsonRpcError::ParseError("x".into()).code(), -32700);
    assert_eq!(JsonRpcError::InvalidRequest("x".into()).code(), -32600);
    assert_eq!(JsonRpcError::MethodNotFound("x".into()).code(), -32601);
    assert_eq!(JsonRpcError::RpcExecution("x".into()).code(), 1);
    assert_eq!(JsonRpcError::InvalidParams("x".into()).code(), -32602);
    assert_eq!(JsonRpcError::InternalError("x".into()).code(), -32603);
    assert_eq!(JsonRpcError::ServerError(-100, "x".into()).code(), -100);
    assert_eq!(JsonRpcError::Unauthorized("x".into()).code(), 1);
}

#[test]
fn test_error_into_response() {
    let err = JsonRpcError::InvalidParams("bad param".to_string());
    let resp = err.into_response(Some(serde_json::json!(1)));
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().message, "bad param");
}

#[test]
fn test_builder_pattern() {
    let req = JsonRpcRequest::new("aria2.tellStatus", serde_json::json!(["abc"])).with_id(5);
    assert_eq!(req.method, "aria2.tellStatus");
    assert_eq!(req.id, Some(serde_json::json!(5)));
}

#[test]
fn test_get_param_or_default() {
    let req = JsonRpcRequest::new("test", serde_json::json!([]));
    let val: String = req.get_param_or_default(0);
    assert!(val.is_empty());
}
