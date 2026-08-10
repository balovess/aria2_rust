//! Process-level coverage for CLI global options inherited by RPC tasks.

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

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

fn wait_for_stopped_task(aria2: &RunningAria2, gid: &str) {
    let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
    loop {
        let stopped = call_jsonrpc(
            aria2,
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.tellStopped",
                "params": [0, 1000],
                "id": "stopped",
            }),
        );
        let is_stopped = stopped["result"].as_array().is_some_and(|results| {
            results
                .iter()
                .any(|result| result["gid"].as_str() == Some(gid))
        });
        if is_stopped {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "task {gid} did not reach stopped results within {PROCESS_EXIT_TIMEOUT:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
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

/// `getGlobalOption` is an original-client contract, not a dump of Rust's
/// internal registry. Hidden original values stay observable when defined,
/// while Rust-only option names cannot leak into browser-client responses.
#[test]
fn e2e_get_global_option_keeps_the_original_wire_surface() {
    let mut aria2 = RunningAria2::start_rpc(&["--enable-utp=true".to_owned()]);

    let response = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.getGlobalOption",
            "params": [],
            "id": "global-options",
        }),
    );
    let options = response["result"]
        .as_object()
        .expect("getGlobalOption must return an object");

    assert_eq!(options.get("dns-timeout"), Some(&json!("30")));
    assert!(
        !options.contains_key("enable-async-dns6"),
        "an original no-default option must not be synthesized"
    );
    assert!(!options.contains_key("enable-utp"));
    assert!(!options.contains_key("utp-listen-port"));

    let change = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.changeGlobalOption",
            "params": [{"enable-async-dns6": "true"}],
            "id": "configure-no-default-option",
        }),
    );
    assert_eq!(change["result"], "OK");

    let configured = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.getGlobalOption",
            "params": [],
            "id": "configured-global-options",
        }),
    );
    assert_eq!(
        configured["result"]["enable-async-dns6"].as_str(),
        Some("true"),
        "a C++ NO_DEFAULT_VALUE preference must be reported once explicitly defined"
    );

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
    let mut aria2 =
        RunningAria2::start_rpc(&[format!("--dir={}", initial_dir.path().display()), uri]);

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

/// Original aria2 preserves the group's `Option` inside `DownloadResult`.
/// Once a CLI-created task is stopped, `getOption` must therefore keep both
/// its creation state and any runtime change that already took effect.
#[test]
fn e2e_stopped_cli_task_get_option_preserves_snapshot_and_applied_changes() {
    let source = TcpListener::bind("127.0.0.1:0").expect("failed to bind source listener");
    let initial_dir = tempdir().expect("failed to create initial directory");
    let uri = format!(
        "http://{}/stopped-cli-task-snapshot.bin",
        source
            .local_addr()
            .expect("source listener must expose its address")
    );
    let mut aria2 =
        RunningAria2::start_rpc(&[format!("--dir={}", initial_dir.path().display()), uri]);

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
            "method": "aria2.changeOption",
            "params": [gid, {"max-download-limit": "2048"}],
            "id": "change-option",
        }),
    );
    assert_eq!(change["result"], "OK");

    let removed = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.forceRemove",
            "params": [gid],
            "id": "force-remove",
        }),
    );
    assert_eq!(removed["result"].as_str(), Some(gid.as_str()));
    wait_for_stopped_task(&aria2, &gid);

    let task_options = call_jsonrpc(
        &aria2,
        json!({
            "jsonrpc": "2.0",
            "method": "aria2.getOption",
            "params": [gid],
            "id": "stopped-task-options",
        }),
    );
    assert_eq!(
        task_options["result"]["dir"].as_str(),
        Some(initial_dir.path().to_string_lossy().as_ref()),
        "stopped CLI task must retain its creation-time directory"
    );
    assert_eq!(
        task_options["result"]["max-download-limit"].as_str(),
        Some("2048"),
        "stopped task must retain only runtime changes that already took effect"
    );

    force_shutdown(&mut aria2);
}
