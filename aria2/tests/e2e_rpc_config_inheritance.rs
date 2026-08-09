//! Process-level coverage for CLI global options inherited by RPC tasks.

mod support;

use std::path::PathBuf;
use std::net::TcpListener;
use std::time::Duration;

use serde_json::{Value, json};
use support::RunningAria2;
use tempfile::tempdir;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn call_jsonrpc(aria2: &RunningAria2, request: Value) -> Value {
    let body = serde_json::to_vec(&request).expect("JSON-RPC request must serialize");
    let response = aria2.post("/jsonrpc", "application/json", &body);
    assert_eq!(response.status, 200, "JSON-RPC must return HTTP 200");
    serde_json::from_slice(&response.body).expect("JSON-RPC response body must be valid JSON")
}

fn add_uri(aria2: &RunningAria2, output_name: &str) -> String {
    let add = call_jsonrpc(
        aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [
                ["http://127.0.0.1:1/config-inheritance.bin"],
                {"out": output_name},
            ],
            "id": "add",
        }),
    );
    add["result"]
        .as_str()
        .expect("addUri must return aria2's string GID")
        .to_owned()
}

fn assert_task_path(aria2: &RunningAria2, gid: &str, dir: &std::path::Path, output_name: &str) {
    let status = call_jsonrpc(
        aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellStatus",
            "params": [gid],
            "id": "status",
        }),
    );
    let result = &status["result"];
    assert_eq!(
        result["dir"].as_str(),
        Some(dir.to_string_lossy().as_ref()),
        "tellStatus must expose the inherited global directory"
    );
    let file_path = result["files"]
        .as_array()
        .and_then(|files| files.first())
        .and_then(|file| file["path"].as_str())
        .expect("tellStatus must expose the task file path");
    assert_eq!(
        PathBuf::from(file_path),
        dir.join(output_name),
        "task-level out must resolve under the inherited global directory"
    );
}

fn force_shutdown(aria2: &mut RunningAria2) {
    let shutdown = call_jsonrpc(
        aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.forceShutdown",
            "params": [],
            "id": "shutdown",
        }),
    );
    assert!(
        shutdown["result"]
            .as_str()
            .is_some_and(|result| result.starts_with("OK.")),
        "forceShutdown must return aria2's successful result: {shutdown}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}

/// `--dir` is a global aria2 option. A task subsequently created through the
/// public JSON-RPC adapter must inherit it, while the per-task `out` option
/// remains the final filename.
#[test]
fn e2e_cli_global_dir_is_inherited_by_jsonrpc_add_uri_task() {
    let download_dir = tempdir().expect("failed to create download directory");
    let mut aria2 = RunningAria2::start_rpc(&[format!("--dir={}", download_dir.path().display())]);

    let gid = add_uri(&aria2, "configured-from-rpc.bin");
    assert_task_path(&aria2, &gid, download_dir.path(), "configured-from-rpc.bin");
    force_shutdown(&mut aria2);
}

/// Runtime global changes and startup configuration share the same aria2
/// global option state. A subsequent RPC-created task must therefore inherit
/// `changeGlobalOption` values as well.
#[test]
fn e2e_change_global_option_dir_is_inherited_by_jsonrpc_add_uri_task() {
    let initial_dir = tempdir().expect("failed to create initial directory");
    let changed_dir = tempdir().expect("failed to create changed directory");
    let mut aria2 = RunningAria2::start_rpc(&[format!("--dir={}", initial_dir.path().display())]);

    let change = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.changeGlobalOption",
            "params": [{"dir": changed_dir.path().to_string_lossy()}],
            "id": "change-dir",
        }),
    );
    assert_eq!(change["result"], "OK");

    let gid = add_uri(&aria2, "runtime-global-dir.bin");
    assert_task_path(&aria2, &gid, changed_dir.path(), "runtime-global-dir.bin");
    force_shutdown(&mut aria2);
}

/// CLI inputs are created before the RPC listener starts. Their `getOption`
/// response must keep the original task option snapshot when a later RPC call
/// changes a global option, just as `aria2_original` serializes the group's
/// own `Option` object rather than the live global one.
#[test]
fn e2e_cli_task_get_option_preserves_initial_snapshot_after_global_change() {
    // Keep the initial CLI download active without external network traffic.
    // The listener accepts the TCP connection but never supplies an HTTP
    // response during this short RPC interaction.
    let source = TcpListener::bind("127.0.0.1:0").expect("failed to bind source listener");
    let initial_dir = tempdir().expect("failed to create initial directory");
    let changed_dir = tempdir().expect("failed to create changed directory");
    let uri = format!(
        "http://{}/cli-task-snapshot.bin",
        source
            .local_addr()
            .expect("source listener must expose its address")
    );
    let mut aria2 = RunningAria2::start_rpc(&[
        format!("--dir={}", initial_dir.path().display()),
        uri,
    ]);

    let active = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.tellActive",
            "params": [],
            "id": "active",
        }),
    );
    let gid = active["result"]
        .as_array()
        .and_then(|groups| groups.first())
        .and_then(|group| group["gid"].as_str())
        .expect("CLI task must remain active while the source listener stalls")
        .to_owned();

    let change = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.changeGlobalOption",
            "params": [{"dir": changed_dir.path().to_string_lossy()}],
            "id": "change-dir",
        }),
    );
    assert_eq!(change["result"], "OK");

    let task_options = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.getOption",
            "params": [gid],
            "id": "task-options",
        }),
    );
    assert_eq!(
        task_options["result"]["dir"].as_str(),
        Some(initial_dir.path().to_string_lossy().as_ref()),
        "getOption for a CLI-created task must not follow a later global mutation"
    );

    force_shutdown(&mut aria2);
}
