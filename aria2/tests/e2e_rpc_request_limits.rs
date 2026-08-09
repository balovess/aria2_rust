//! Process-level HTTP request-size compatibility coverage.

mod support;

use std::time::Duration;

use serde_json::json;
use support::RunningAria2;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// `HttpServerCommand` checks Content-Length before it schedules the body
/// parser. When the configured cap is exceeded, aria2_original drops the
/// connection without writing an HTTP status or JSON-RPC error response.
#[test]
fn e2e_http_oversized_rpc_request_drops_connection_without_response() {
    let mut aria2 = RunningAria2::start_rpc(&["--rpc-max-request-size=1K".to_owned()]);

    let response = aria2.post_head_only_until_close("/jsonrpc", "application/json", 1025);
    assert!(
        response.is_empty(),
        "aria2_original closes an oversized request without an HTTP response, got: {}",
        String::from_utf8_lossy(&response)
    );

    let shutdown_body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "method": "aria2.forceShutdown",
        "params": [],
        "id": "shutdown",
    }))
    .expect("shutdown JSON-RPC request must serialize");
    let shutdown = aria2.post("/jsonrpc", "application/json", &shutdown_body);
    assert_eq!(shutdown.status, 200);
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}

/// HTTP authentication precedes the original request-size check. An
/// unauthenticated client therefore receives the Basic challenge even when
/// it advertises an oversized request body.
#[test]
fn e2e_http_oversized_unauthorized_request_returns_basic_challenge() {
    let aria2 = RunningAria2::start_rpc(&[
        "--rpc-user=aria2".to_owned(),
        "--rpc-passwd=secret".to_owned(),
        "--rpc-max-request-size=1K".to_owned(),
    ]);

    let response = aria2.post_head_only_until_close("/jsonrpc", "application/json", 1025);
    let response = String::from_utf8(response).expect("HTTP response must be UTF-8");
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "authentication must run before the request-size close path: {response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("www-authenticate: basic realm=\"aria2\""),
        "original Basic challenge must be preserved: {response}"
    );
}
