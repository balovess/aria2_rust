//! End-to-end tests for the RPC HTTP server.
//!
//! Each test starts a fresh server on a random port, exercises one or
//! more JSON-RPC / WebSocket operations, and verifies the responses.
//!
//! Groups:
//!   A — Basic routing        (9 tests)
//!   B — Authentication       (4 tests)
//!   C — CORS                 (3 tests)
//!   D — WebSocket            (5 tests)
//!   E — Batch requests       (2 tests)
//!   F — Full lifecycle       (3 tests)

mod common;

use common::{start_test_server, start_test_server_with_config};
use std::time::Duration;

use aria2_rpc::server::{AuthConfig, CorsConfig, ServerConfig};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a JSON-RPC POST body.
fn rpc_body(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "id": method.replace('.', "-"),
        "params": params,
    })
}

/// Send a JSON-RPC request and return the (status, JSON response).
async fn rpc_call_with_status(
    client: &Client,
    base_url: &str,
    method: &str,
    params: Value,
) -> (reqwest::StatusCode, Value) {
    let resp = client
        .post(format!("{base_url}/jsonrpc"))
        .json(&rpc_body(method, params))
        .send()
        .await
        .expect("POST /jsonrpc failed");
    let status = resp.status();
    let body: Value = resp.json().await.expect("invalid JSON response");
    (status, body)
}

/// Send a JSON-RPC request, assert 200, return the JSON response.
async fn rpc_call(client: &Client, base_url: &str, method: &str, params: Value) -> Value {
    let (status, body) = rpc_call_with_status(client, base_url, method, params).await;
    assert_eq!(
        status, 200,
        "expected 200 for {method}, got {status}: {body}"
    );
    body
}

/// Assert the JSON-RPC response is a success with a non-null result.
fn assert_result(resp: &Value) {
    assert!(
        resp.get("result").is_some(),
        "expected 'result' field, got: {resp}"
    );
    assert!(
        !resp["result"].is_null(),
        "expected non-null result, got null"
    );
}

/// Assert the JSON-RPC response contains an error with the given code.
fn assert_error_code(resp: &Value, code: i64) {
    let err = resp
        .get("error")
        .unwrap_or_else(|| panic!("expected 'error' field, got: {resp}"));
    assert_eq!(
        err["code"].as_i64().unwrap_or_default(),
        code,
        "expected error code {code}, got: {err}"
    );
}

/// Parse the GID from an `aria2.addUri` result.
fn parse_gid(resp: &Value) -> String {
    resp["result"]
        .as_str()
        .expect("result should be a string (GID)")
        .to_string()
}

// =========================================================================
// Group A — Basic routing
// =========================================================================

#[tokio::test]
async fn e2e_root_endpoint_matches_original_not_found_contract() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client.get(&base).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn e2e_jsonrpc_get_without_query_matches_original() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client.get(format!("{base}/jsonrpc")).send().await.unwrap();
    assert_eq!(resp.status(), 500);

    let body: Value = resp.json().await.unwrap();
    assert_error_code(&body, -32700);
    assert_eq!(body["error"]["message"], "Parse error.");
}

#[tokio::test]
async fn e2e_jsonrpc_get_query_compatibility() {
    use base64::Engine;

    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let params = base64::engine::general_purpose::STANDARD.encode("[]");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("method", "aria2.getVersion")
        .append_pair("id", "get-1")
        .append_pair("params", &params)
        .finish();

    let response = client
        .get(format!("{base}/jsonrpc?{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "application/json-rpc"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["id"], "get-1");
    assert_result(&body);
}

#[tokio::test]
async fn e2e_jsonp_get_query_compatibility() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let params = base64::engine::general_purpose::STANDARD.encode("[]");
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("method", "aria2.getVersion")
        .append_pair("id", "jsonp-1")
        .append_pair("params", &params)
        .append_pair("jsoncallback", "aria2Callback")
        .finish();

    let response = client
        .get(format!("{base}/jsonrpc?{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "text/javascript"
    );
    let body = response.text().await.unwrap();
    assert!(body.starts_with("aria2Callback({"));
    assert!(body.ends_with("})"));
    assert!(body.contains("\"id\":\"jsonp-1\""));
}

#[tokio::test]
async fn e2e_jsonrpc_get_batch_query_compatibility() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion", "id": "b1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "id": "b2", "params": []}
    ]);
    let params =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&batch).unwrap());

    let response = client
        .get(format!("{base}/jsonrpc?params={params}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body.as_array().map(Vec::len), Some(2));
    assert_eq!(body[0]["id"], "b1");
    assert_eq!(body[1]["id"], "b2");
}

#[tokio::test]
async fn e2e_jsonrpc_get_errors_use_compatible_status_codes() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let invalid_base64 = client
        .get(format!(
            "{base}/jsonrpc?method=aria2.getVersion&params=not-base64"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_base64.status(), 500);
    let invalid_base64_body: Value = invalid_base64.json().await.unwrap();
    assert_error_code(&invalid_base64_body, -32700);

    let raw_callback = client
        .get(format!(
            "{base}/jsonrpc?method=aria2.getVersion&params=e30%3D&jsoncallback=bad;alert"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(raw_callback.status(), 400);
    assert_eq!(
        raw_callback.headers()[reqwest::header::CONTENT_TYPE],
        "text/javascript"
    );
    let raw_callback_body = raw_callback.text().await.unwrap();
    assert!(raw_callback_body.starts_with("bad;alert({"));
    assert!(raw_callback_body.contains("\"code\":-32600"));
}

#[tokio::test]
async fn e2e_add_uri_via_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test"]]),
    )
    .await;

    // The download will fail because the URL is unreachable, but a GID
    // should still be returned (the download is created immediately).
    let gid = resp["result"].as_str();
    assert!(gid.is_some(), "expected a GID string, got: {resp}");
    assert_eq!(gid.unwrap().len(), 16, "GID must be 16 hex chars");
}

#[tokio::test]
async fn e2e_get_version_via_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;
    assert_result(&resp);
    assert!(resp["result"]["version"].is_string());
}

#[tokio::test]
async fn e2e_get_global_stat() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getGlobalStat", json!([])).await;
    assert_result(&resp);
    assert!(resp["result"]["downloadSpeed"].is_string());
}

#[tokio::test]
async fn e2e_rpc_endpoint_post() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let body = r#"<?xml version="1.0"?><methodCall><methodName>aria2.getVersion</methodName><params/></methodCall>"#;

    let response = client
        .post(format!("{base}/rpc"))
        .header("content-type", "text/xml")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("<methodResponse>"));
    assert!(body.contains("<name>version</name>"));
}

#[tokio::test]
async fn e2e_invalid_endpoint() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .get(format!("{base}/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn e2e_post_invalid_json() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .body("not-json-at-all")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    // aria2 serializes the JSON-RPC parse error, but reports it as a server
    // error at the HTTP layer.
    assert_eq!(resp.status(), 500);
    let body: Value = resp.json().await.unwrap();
    assert_error_code(&body, -32700);
}

#[tokio::test]
async fn e2e_jsonrpc_errors_close_the_http_connection_like_original() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let response = client
        .post(format!("{base}/jsonrpc"))
        .header("content-type", "application/json")
        .body("not-json-at-all")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 500);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONNECTION)
            .and_then(|value| value.to_str().ok()),
        Some("close")
    );
}

#[tokio::test]
async fn e2e_jsonrpc_batch_errors_keep_the_http_connection_like_original() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let response = client
        .post(format!("{base}/jsonrpc"))
        .json(&json!([
            rpc_body("aria2.getVersion", json!([])),
            rpc_body("aria2.nonexistentMethod", json!([])),
        ]))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_ne!(
        response
            .headers()
            .get(reqwest::header::CONNECTION)
            .and_then(|value| value.to_str().ok()),
        Some("close")
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body.as_array().map(Vec::len), Some(2));
    assert_result(&body[0]);
    assert_error_code(&body[1], 1);
}

#[tokio::test]
async fn e2e_xmlrpc_get_version() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let body = r#"<?xml version="1.0"?><methodCall><methodName>aria2.getVersion</methodName><params/></methodCall>"#;

    let response = client
        .post(format!("{base}/rpc"))
        .header("content-type", "text/xml")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/xml")
    );
    let text = response.text().await.unwrap();
    assert!(text.contains("<methodResponse>"));
    assert!(text.contains("<name>version</name>"));
}

#[tokio::test]
async fn e2e_xmlrpc_execution_errors_use_aria2_fault_code_one() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let body = r#"<?xml version="1.0"?><methodCall><methodName>aria2.addUri</methodName><params><param><value><int>7</int></value></param></params></methodCall>"#;

    let response = client
        .post(format!("{base}/rpc"))
        .header("content-type", "text/xml")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("<name>faultCode</name>"));
    assert!(text.contains("<int>1</int>"));
}

#[tokio::test]
async fn e2e_xmlrpc_parse_errors_match_original_http_contract() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();
    let malformed = r#"<?xml version="1.0"?><methodCall><methodName>aria2.getVersion</methodName><params></methodCall>"#;

    let response = client
        .post(format!("{base}/rpc"))
        .header("content-type", "text/xml")
        .body(malformed)
        .send()
        .await
        .unwrap();

    // aria2_original calls feedResponse(400) on XML parser failure: no XML
    // fault document and no content type are emitted.
    assert_eq!(response.status(), 400);
    assert!(!response.headers().contains_key("content-type"));
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn e2e_unknown_rpc_method() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let (status, resp) =
        rpc_call_with_status(&client, &base, "aria2.nonexistentMethod", json!([])).await;
    assert_eq!(status, 400);
    assert_error_code(&resp, 1);
}

// =========================================================================
// Group B — Authentication
// =========================================================================

const TEST_TOKEN: &str = "my-secret-token";

fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

#[tokio::test]
async fn e2e_basic_auth_protects_json_xml_and_preflight() {
    let config = ServerConfig::default()
        .with_auth(AuthConfig::default().with_basic_auth("aria2", "basic-secret"));
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let denied = client
        .post(format!("{base}/jsonrpc"))
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(
        denied
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some("Basic realm=\"aria2\"")
    );

    let authorized = client
        .post(format!("{base}/jsonrpc"))
        .header(
            reqwest::header::AUTHORIZATION,
            basic_auth_header("aria2", "basic-secret"),
        )
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), 200);
    let authorized_body: Value = authorized.json().await.unwrap();
    assert_result(&authorized_body);

    let xml = r#"<?xml version="1.0"?><methodCall><methodName>aria2.getVersion</methodName><params/></methodCall>"#;
    let xml_denied = client
        .post(format!("{base}/rpc"))
        .header("content-type", "text/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(xml_denied.status(), 401);

    let xml_authorized = client
        .post(format!("{base}/rpc"))
        .header(
            reqwest::header::AUTHORIZATION,
            basic_auth_header("aria2", "basic-secret"),
        )
        .header("content-type", "text/xml")
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(xml_authorized.status(), 200);

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 200);
}

#[tokio::test]
async fn e2e_basic_auth_protects_websocket_upgrade() {
    let config = ServerConfig::default()
        .with_auth(AuthConfig::default().with_basic_auth("aria2", "basic-secret"));
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let ws_url = base.replace("http://", "ws://");

    let denied = connect_async(format!("{ws_url}/jsonrpc")).await;
    assert!(
        denied.is_err(),
        "unauthenticated WebSocket upgrade must fail"
    );

    let mut request = format!("{ws_url}/jsonrpc").into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        basic_auth_header("aria2", "basic-secret").parse().unwrap(),
    );
    let (socket, _) = connect_async(request).await.unwrap();
    drop(socket);
}

#[tokio::test]
async fn e2e_auth_valid_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let resp = rpc_call(
        &client,
        &base,
        "aria2.getVersion",
        json![["token:my-secret-token"]],
    )
    .await;
    assert_result(&resp);
}

#[tokio::test]
async fn e2e_auth_wrong_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let (status, resp) = rpc_call_with_status(
        &client,
        &base,
        "aria2.getVersion",
        json![["token:wrong-token"]],
    )
    .await;
    assert_eq!(status, 400);
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_auth_no_token() {
    let (base, _guard) = start_test_server(Some(TEST_TOKEN)).await;
    let client = Client::new();

    let (status, resp) = rpc_call_with_status(&client, &base, "aria2.getVersion", json!([])).await;
    assert_eq!(status, 400);
    assert_error_code(&resp, 1);
}

#[tokio::test]
async fn e2e_auth_no_auth_required() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = rpc_call(&client, &base, "aria2.getVersion", json!([])).await;
    assert_result(&resp);
}

// =========================================================================
// Group C — CORS
// =========================================================================

#[tokio::test]
async fn e2e_cors_preflight() {
    let config = ServerConfig::default().with_cors(CorsConfig::allow_all_origins());
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_some());
    assert_eq!(
        resp.headers()
            .get("access-control-max-age")
            .and_then(|value| value.to_str().ok()),
        Some("1728000"),
        "CORS preflight cache lifetime must match aria2_original"
    );
}

#[tokio::test]
async fn e2e_cors_is_disabled_by_default_like_aria2_original() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "aria2_original only emits CORS headers when rpc-allow-origin-all is enabled"
    );
    assert!(resp.headers().get("access-control-max-age").is_none());
}

#[tokio::test]
async fn e2e_cors_allowed_origin() {
    let config = ServerConfig::default().with_cors(CorsConfig::allow_all_origins());
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://example.com")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(resp.headers().get("access-control-allow-origin").is_some());
}

/// With `--rpc-allow-origin-all=true`, aria2_original writes the wildcard CORS
/// header on every RPC response, including requests that do not send Origin.
#[tokio::test]
async fn e2e_cors_wildcard_is_emitted_without_request_origin_like_original() {
    let config = ServerConfig::default().with_cors(CorsConfig::allow_all_origins());
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "aria2_original emits its configured wildcard header independently of request Origin"
    );
}

#[tokio::test]
async fn e2e_cors_wildcard() {
    let config = ServerConfig::default().with_cors(CorsConfig::allow_all_origins());
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://random-origin.example")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn e2e_cors_uses_restricted_origin_and_preflight_config() {
    let cors = CorsConfig::from_option_value("https://allowed.example");
    let config = ServerConfig::default().with_cors(cors);
    let (base, _guard) = start_test_server_with_config(None, 5, config).await;
    let client = Client::new();

    let allowed = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://allowed.example")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(
        allowed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://allowed.example")
    );

    let blocked = client
        .post(format!("{base}/jsonrpc"))
        .header("Origin", "https://blocked.example")
        .json(&rpc_body("aria2.getVersion", json!([])))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), 200);
    assert!(
        blocked
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://allowed.example")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "content-type, authorization",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 200);
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://allowed.example")
    );
    assert!(
        preflight
            .headers()
            .get("access-control-allow-methods")
            .is_some()
    );

    let blocked_preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/jsonrpc"))
        .header("Origin", "https://blocked.example")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(blocked_preflight.status(), 200);
    assert!(
        blocked_preflight
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
}

// =========================================================================
// Group D — WebSocket
// =========================================================================

#[tokio::test]
async fn e2e_ws_upgrade_at_non_original_path_is_rejected() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    assert!(
        connect_async(format!("{ws_url}/ws")).await.is_err(),
        "upstream aria2 accepts WebSocket upgrades only at /jsonrpc"
    );
}

#[tokio::test]
async fn e2e_ws_upgrade_at_jsonrpc() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WebSocket upgrade at /jsonrpc should succeed");
    let (_, rx) = ws.split();
    drop(rx);
}

#[tokio::test]
async fn e2e_ws_receive_event_add() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // Connect WS first
    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Trigger a download via HTTP
    let resp = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test-event-add"]]),
    )
    .await;
    let _gid = parse_gid(&resp);

    // We should receive at least one event (onDownloadStart)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after addUri")
        .expect("WS stream ended")
        .expect("WS message error");
}

#[tokio::test]
async fn e2e_ws_receive_event_pause() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // Connect WS
    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Add a download and pause it
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test-event-pause"]]),
    )
    .await;
    let gid = parse_gid(&add);

    let _pause = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;

    // Expect at least one event (onDownloadPause)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after pause")
        .expect("WS stream ended")
        .expect("WS message error");
}

#[tokio::test]
async fn e2e_ws_receive_event_complete() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (_, mut rx) = ws.split();

    // Removing a download should produce onDownloadStop/Complete.
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/test-event-remove"]]),
    )
    .await;
    let gid = parse_gid(&add);

    let _remove = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;

    // Expect at least one event (onDownloadStop or onDownloadComplete)
    tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after remove")
        .expect("WS stream ended")
        .expect("WS message error");
}

// =========================================================================
// Group G — WebSocket JSON-RPC request/response
// =========================================================================

/// Verify that a WebSocket client can send a JSON-RPC request and receive
/// a response on the same connection (matching C++ aria2's
/// `WebSocketSession::onMsgRecvCallback` behavior).
#[tokio::test]
async fn e2e_ws_jsonrpc_get_version() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // Send aria2.getVersion over WebSocket
    use tokio_tungstenite::tungstenite::Message;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": 1
    });
    tx.send(Message::Text(request.to_string()))
        .await
        .expect("send failed");

    // Receive and parse the response
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("response should be valid JSON");

    // Verify it is a JSON-RPC success response
    assert!(
        resp.get("result").is_some(),
        "expected 'result' field in WS response, got: {resp}"
    );
    assert_eq!(resp["id"], 1, "response id should match request id");
    assert!(
        resp["result"]["version"].is_string(),
        "version should be a string"
    );
}

/// C++ aria2 hands every non-control WebSocket frame to its JSON parser. A
/// binary frame containing a valid JSON-RPC request must therefore receive the
/// same text JSON-RPC response as the equivalent text frame.
#[tokio::test]
async fn e2e_ws_binary_jsonrpc_request_matches_original_non_control_frame_behavior() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    use tokio_tungstenite::tungstenite::Message;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": "binary-frame",
    });
    tx.send(Message::Binary(request.to_string().into_bytes()))
        .await
        .expect("send binary JSON-RPC request failed");

    let message = tokio::time::timeout(Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for binary-frame JSON-RPC response")
        .expect("WS stream ended")
        .expect("WS message error");
    let response: Value = serde_json::from_str(
        &message
            .into_text()
            .expect("original-compatible response must be a text frame"),
    )
    .expect("response should be valid JSON");

    assert_eq!(response["id"], "binary-frame");
    assert!(
        response["result"]["version"].is_string(),
        "expected getVersion response, got: {response}"
    );
}

/// Verify that a WebSocket client can send a batch JSON-RPC request and
/// receive an array of responses.
#[tokio::test]
async fn e2e_ws_jsonrpc_batch_request() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // Send batch request with two methods
    use tokio_tungstenite::tungstenite::Message;
    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion", "params": [], "id": "b1"},
        {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "params": [], "id": "b2"},
    ]);
    tx.send(Message::Text(batch.to_string()))
        .await
        .expect("send failed");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS batch response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Vec<Value> =
        serde_json::from_str(&text).expect("batch response should be a JSON array");

    assert_eq!(resp.len(), 2, "batch response should contain 2 items");
    assert!(
        resp[0]["result"].is_object(),
        "first result should be an object"
    );
    assert!(
        resp[1]["result"].is_object(),
        "second result should be an object"
    );
    assert_eq!(resp[0]["id"], "b1");
    assert_eq!(resp[1]["id"], "b2");
}

/// Verify that an invalid JSON message over WebSocket returns a proper
/// JSON-RPC Parse Error (-32700) response.
#[tokio::test]
async fn e2e_ws_jsonrpc_invalid_json() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");

    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    use tokio_tungstenite::tungstenite::Message;
    tx.send(Message::Text("{not valid json}".into()))
        .await
        .expect("send failed");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS error response")
        .expect("WS stream ended")
        .expect("WS message error");

    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("error response should be valid JSON");

    assert!(resp.get("error").is_some(), "expected 'error' field");
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32700),
        "expected Parse Error code -32700, got: {resp}"
    );
}

/// Verify that event notifications continue flowing while the WS connection
/// is processing JSON-RPC requests.
#[tokio::test]
async fn e2e_ws_jsonrpc_with_events() {
    let (base, _guard) = start_test_server(None).await;
    let ws_url = base.replace("http://", "ws://");
    let client = Client::new();

    // 1. Connect WS
    let (ws, _) = connect_async(format!("{ws_url}/jsonrpc"))
        .await
        .expect("WS upgrade failed");
    let (mut tx, mut rx) = ws.split();

    // 2. Send a JSON-RPC request over WS
    use tokio_tungstenite::tungstenite::Message;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": "ev-test"
    });
    tx.send(Message::Text(request.to_string()))
        .await
        .expect("send failed");

    // 3. Receive the JSON-RPC response
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.next())
        .await
        .expect("timeout waiting for WS JSON-RPC response")
        .expect("WS stream ended")
        .expect("WS message error");
    let text = msg.into_text().expect("expected text message");
    let resp: Value = serde_json::from_str(&text).expect("response should be valid JSON");
    assert!(
        resp.get("result").is_some(),
        "expected 'result' in JSON-RPC response, got: {resp}"
    );

    // 4. Trigger a download event via HTTP
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/ws-event-test"]]),
    )
    .await;
    let _gid = parse_gid(&add);

    // 5. Verify we still receive events on the same WS connection
    let event_msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.next())
        .await
        .expect("timeout waiting for WS event after addUri")
        .expect("WS stream ended")
        .expect("WS message error");
    let event_text = event_msg.into_text().expect("expected text message");
    let event: Value = serde_json::from_str(&event_text).expect("event should be valid JSON");
    // Event notifications have a "method" field (e.g. "aria2.onDownloadStart")
    assert!(
        event.get("method").is_some(),
        "expected 'method' field in event notification, got: {event}"
    );
}

// =========================================================================
// Group E — Batch requests
// =========================================================================

#[tokio::test]
async fn e2e_batch_valid() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion",  "id": "v1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "id": "g1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.getSessionInfo", "id": "s1", "params": []},
    ]);

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn e2e_batch_mixed() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let batch = json!([
        {"jsonrpc": "2.0", "method": "aria2.getVersion",       "id": "v1", "params": []},
        {"jsonrpc": "2.0", "method": "aria2.nonexistentMethod", "id": "e1", "params": []},
    ]);

    let resp = client
        .post(format!("{base}/jsonrpc"))
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body.as_array().map(Vec::len), Some(2));
    assert_result(&body[0]);
    assert_error_code(&body[1], 1);
}

#[tokio::test]
async fn e2e_jsonrpc_matches_original_wire_envelope_rules() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // aria2_original ignores the jsonrpc member and defaults omitted params
    // to an empty positional list.
    let compatible_object = client
        .post(format!("{base}/jsonrpc"))
        .json(&json!({
            "jsonrpc": "1.0",
            "id": "compat-1",
            "method": "aria2.getVersion"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(compatible_object.status(), 200);
    let compatible_body: Value = compatible_object.json().await.unwrap();
    assert_result(&compatible_body);
    assert_eq!(compatible_body["id"], "compat-1");

    // Object-level failures are returned as entries; non-object batch items
    // are skipped exactly as in HttpServerBodyCommand.cc.
    let mixed = client
        .post(format!("{base}/jsonrpc"))
        .json(&json!([
            42,
            {"id": 1, "method": "aria2.getVersion", "params": {}},
            {"method": "aria2.getVersion"},
            {"id": 2, "method": "aria2.getVersion", "params": []}
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(mixed.status(), 200);
    let mixed_body: Value = mixed.json().await.unwrap();
    assert_eq!(mixed_body.as_array().map(Vec::len), Some(3));
    assert_error_code(&mixed_body[0], -32602);
    assert_error_code(&mixed_body[1], -32600);
    assert_result(&mixed_body[2]);

    let empty_batch = client
        .post(format!("{base}/jsonrpc"))
        .json(&json!([]))
        .send()
        .await
        .unwrap();
    assert_eq!(empty_batch.status(), 200);
    let empty_body: Value = empty_batch.json().await.unwrap();
    assert_eq!(empty_body, json!([]));
}

// =========================================================================
// Group F — Full lifecycle
// =========================================================================

/// Full lifecycle test: add → status → pause → unpause → remove → tellStatus returns "removed".
#[tokio::test]
async fn e2e_full_lifecycle() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // 1. Add a download
    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/lifecycle-test"]]),
    )
    .await;
    let gid = parse_gid(&add);
    assert_eq!(gid.len(), 16, "GID must be 16 hex chars");

    // 2. tellStatus — the download exists
    let status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_result(&status);
    assert_eq!(status["result"]["gid"].as_str(), Some(gid.as_str()));

    // 3. Pause — C++ returns the GID as a string (not array)
    let pause = rpc_call(&client, &base, "aria2.pause", json![[&gid]]).await;
    assert_result(&pause);
    assert_eq!(
        pause["result"].as_str(),
        Some(gid.as_str()),
        "pause should return the GID string"
    );

    // 4. tellStatus — paused
    let paused = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
    assert_result(&paused);
    let status_str = paused["result"]["status"].as_str().unwrap_or("");
    assert!(
        status_str == "paused",
        "expected paused status, got '{status_str}'"
    );

    // 5. Unpause
    let unpause = rpc_call(&client, &base, "aria2.unpause", json![[&gid]]).await;
    assert_result(&unpause);

    // 6. Remove
    let remove = rpc_call(&client, &base, "aria2.remove", json![[&gid]]).await;
    assert_result(&remove);

    // 7. tellStatus for removed GID — C++ aria2 keeps removed downloads in
    // DownloadResult (stopped list) so tellStatus returns status="removed"
    // rather than an error. Only errors if the GID was never added or
    // has been purged via removeDownloadResult/purgeDownloadResult.
    // After remove, the download transitions through intermediate states
    // ("waiting", "active") before reaching a terminal state ("removed" or
    // "error"). We poll until terminal — the intermediate state depends on
    // engine timing and must not be asserted strictly (flaky on fast CI).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let removed_status = rpc_call(&client, &base, "aria2.tellStatus", json![[&gid]]).await;
        if let Some(result) = removed_status.get("result") {
            let status = result.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "removed" || status == "error" {
                break;
            }
        } else if removed_status.get("error").is_some() {
            break;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "removed download did not reach a terminal state: {removed_status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn e2e_global_option_change() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    // 1. Get current global option
    let before = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_result(&before);

    // 2. Change max concurrent downloads — params must be [{...}]
    let change = rpc_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json![[{"max-concurrent-downloads": "5"}]],
    )
    .await;
    assert_result(&change);

    // 3. Verify the change is reflected
    let after = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_result(&after);
    assert_eq!(
        after["result"]["max-concurrent-downloads"].as_str(),
        Some("5"),
        "expected max-concurrent-downloads=5, got: {}",
        after["result"]["max-concurrent-downloads"]
    );
}

/// `aria2.getOption` serializes the task's own option state. A later global
/// change must affect future tasks only, not rewrite an existing task's
/// response over the public HTTP JSON-RPC adapter.
#[tokio::test]
async fn e2e_get_option_preserves_task_snapshot_after_global_change() {
    let (base, _guard) = start_test_server(None).await;
    let client = Client::new();

    let add = rpc_call(
        &client,
        &base,
        "aria2.addUri",
        json!([["http://127.0.0.1:1/task-snapshot.bin"]]),
    )
    .await;
    let gid = parse_gid(&add);

    let change = rpc_call(
        &client,
        &base,
        "aria2.changeGlobalOption",
        json!([{"dir": "/tmp/http-task-snapshot"}]),
    )
    .await;
    assert_eq!(change["result"], "OK");

    let global = rpc_call(&client, &base, "aria2.getGlobalOption", json!([])).await;
    assert_eq!(global["result"]["dir"], "/tmp/http-task-snapshot");

    let task = rpc_call(&client, &base, "aria2.getOption", json!([gid])).await;
    assert_eq!(
        task["result"]["dir"], ".",
        "the original task snapshot must not follow a later global mutation"
    );
}
