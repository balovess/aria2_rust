//! Process-level compatibility coverage for AriaNg's JSON-RPC refresh flow.

mod support;

use std::time::Duration;

use serde_json::{Value, json};
use support::RunningAria2;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn call_jsonrpc(aria2: &RunningAria2, request: Value) -> Value {
    let body = serde_json::to_vec(&request).expect("JSON-RPC request must serialize");
    let response = aria2.post("/jsonrpc", "application/json", &body);
    assert_eq!(
        response.status, 200,
        "AriaNg refresh request must return HTTP 200, got: {}",
        response.headers
    );
    serde_json::from_slice(&response.body).expect("JSON-RPC response body must be valid JSON")
}

/// AriaNg sends `system.multicall` without an envelope token and puts
/// `token:<secret>` at the beginning of every sub-call parameter list. Verify
/// the exact request shape against a live aria2c process, including the CLI
/// RPC setup, HTTP wire adapter, multicall authorization, and clean shutdown.
#[test]
fn e2e_arianng_multicall_refreshes_live_rpc_process_with_per_call_token() {
    let secret = "arianng-process-secret";
    let mut aria2 = RunningAria2::start_rpc(&[format!("--rpc-secret={secret}")]);

    let refresh = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "system.multicall",
            "id": "arianng-refresh",
            "params": [[
                {"methodName": "aria2.tellActive", "params": [format!("token:{secret}")]},
                {"methodName": "aria2.tellWaiting", "params": [format!("token:{secret}"), 0, 1000]},
                {"methodName": "aria2.tellStopped", "params": [format!("token:{secret}"), 0, 1000]},
                {"methodName": "aria2.getGlobalStat", "params": [format!("token:{secret}")]},
            ]],
        }),
    );

    assert_eq!(refresh["jsonrpc"], "2.0");
    assert_eq!(refresh["id"], "arianng-refresh");
    let entries = refresh["result"]
        .as_array()
        .expect("multicall refresh must return an array");
    assert_eq!(entries.len(), 4);
    for (index, entry) in entries.iter().take(3).enumerate() {
        assert!(
            entry
                .as_array()
                .and_then(|result| result.first())
                .is_some_and(Value::is_array),
            "refresh entry {index} must preserve aria2's nested result array: {entry}"
        );
    }
    let global_stat = entries[3]
        .as_array()
        .and_then(|result| result.first())
        .expect("getGlobalStat must have a multicall result wrapper");
    assert!(
        global_stat["downloadSpeed"].is_string(),
        "AriaNg expects stringified global statistics: {global_stat}"
    );

    let shutdown = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.shutdown",
            "id": "shutdown",
            "params": [format!("token:{secret}")],
        }),
    );
    assert_eq!(shutdown["id"], "shutdown");
    assert!(
        shutdown["result"]
            .as_str()
            .is_some_and(|result| result.starts_with("OK.")),
        "shutdown must return aria2's successful result: {shutdown}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}
