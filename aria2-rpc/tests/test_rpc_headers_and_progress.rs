//! Integration test: RPC HTTP header passing and live progress feedback.
//!
//! Tests the three RPC fixes:
//! 1. `aria2.addUri` with `header`/`user-agent`/`referer` options → headers
//!    correctly stored in the `RequestGroup`'s `DownloadOptions`.
//! 2. `aria2.tellStatus` reads live progress (total/completed/speed) from
//!    `RequestGroupMan` atomic fields — not placeholder data.
//! 3. `aria2.getGlobalStat` / `aria2.tellActive` aggregate live data from
//!    all registered groups.

use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

// =========================================================================
// Test Helpers
// =========================================================================

/// Create an `RpcEngine` wired to a real `RequestGroupMan` + engine command channel,
/// simulating the shared-state setup that the app uses in RPC mode.
fn create_engine_with_shared_state() -> (
    RpcEngine,
    Arc<RequestGroupMan>,
    mpsc::UnboundedReceiver<EngineCommand>,
) {
    let group_man = Arc::new(RequestGroupMan::new());
    let (engine_cmd_tx, engine_cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
    let engine = RpcEngine::new()
        .with_group_man(group_man.clone())
        .with_engine_cmd_tx(engine_cmd_tx);
    (engine, group_man, engine_cmd_rx)
}

// =========================================================================
// Test 1: HTTP headers passed from RPC options to RequestGroup
// =========================================================================

#[tokio::test]
async fn test_add_uri_with_array_headers_stored_in_group() {
    let (engine, group_man, mut cmd_rx) = create_engine_with_shared_state();

    // Send aria2.addUri with array-form headers and other options
    let req = JsonRpcRequest::new(
        "aria2.addUri",
        json!([
            ["http://example.com/file.zip"],
            {
                "header": ["Referer: https://example.com", "User-Agent: TestAgent/1.0"],
                "user-agent": "MyCustomUA",
                "referer": "https://referer.example.com",
                "dir": "/tmp/downloads",
                "out": "file.zip",
                "split": 8,
                "max-connection-per-server": 4
            }
        ]),
    )
    .with_id(1);

    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "addUri should succeed");
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(gid.len(), 16, "GID should be 16 hex chars");

    // Verify an EngineCommand::AddDownload was dispatched to the v2 engine channel
    let command = tokio::time::timeout(std::time::Duration::from_millis(500), cmd_rx.recv())
        .await
        .expect("EngineCommand should have been sent to engine_cmd_tx")
        .expect("channel should not be closed");
    assert!(matches!(command, EngineCommand::AddDownload { .. }));

    // Verify the RequestGroup has the correct options stored
    let man = &group_man;
    let group_lock = man
        .group_by_hex(&gid)
        .expect("group should be registered in RequestGroupMan");
    let g = group_lock.read().unwrap();
    let opts = g.options();

    assert_eq!(
        opts.header,
        vec!["Referer: https://example.com", "User-Agent: TestAgent/1.0"]
    );
    assert_eq!(opts.user_agent.as_deref(), Some("MyCustomUA"));
    assert_eq!(opts.referer.as_deref(), Some("https://referer.example.com"));
    assert_eq!(opts.dir.as_deref(), Some("/tmp/downloads"));
    assert_eq!(opts.out.as_deref(), Some("file.zip"));
    assert_eq!(opts.split, Some(8));
    assert_eq!(opts.max_connection_per_server, Some(4));
}

// =========================================================================
// Test 2: String-form headers (newline-separated) parsed correctly
// =========================================================================

#[tokio::test]
async fn test_add_uri_with_string_headers_stored_in_group() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Send aria2.addUri with string-form header (newline-separated)
    let req = JsonRpcRequest::new(
        "aria2.addUri",
        json!([
            ["http://example.com/file.zip"],
            {
                "header": "Referer: https://example.com\nUser-Agent: TestAgent/1.0\n"
            }
        ]),
    )
    .with_id(1);

    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    let man = &group_man;
    let group_lock = man.group_by_hex(&gid).expect("group should exist");
    let g = group_lock.read().unwrap();
    let opts = g.options();

    // Newline-separated string should be split into individual headers
    assert_eq!(
        opts.header,
        vec!["Referer: https://example.com", "User-Agent: TestAgent/1.0"]
    );
}

// =========================================================================
// Test 3: tellStatus returns live progress from RequestGroupMan
// =========================================================================

#[tokio::test]
async fn test_tell_status_returns_live_progress() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Add a download
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        json!([["http://example.com/largefile.bin"]]),
    )
    .with_id(1);
    let resp = engine.handle_request(&add_req).await;
    assert!(resp.is_success());
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Before progress update: status should be Waiting, progress zero
    let tell_req = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(2);
    let resp = engine.handle_request(&tell_req).await;
    assert!(resp.is_success());
    let status = resp.result.unwrap();
    assert_eq!(status["gid"], gid);
    assert_eq!(status["status"], "waiting");

    // Simulate download progress: set total, completed, speed, and start
    {
        let man = &group_man;
        let group_lock = man.group_by_hex(&gid).unwrap();
        let mut g = group_lock.write().unwrap();
        g.start().unwrap(); // Status → Active
        g.set_total_length_atomic(10_000_000);
        g.set_completed_length(4_200_000);
        g.set_download_speed_cached(512_000);
    }

    // Query tellStatus again — should return live progress from RequestGroup
    let tell_req = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(3);
    let resp = engine.handle_request(&tell_req).await;
    assert!(resp.is_success());

    let status = resp.result.unwrap();
    assert_eq!(status["gid"], gid);
    assert_eq!(status["status"], "active");
    // Wire format: all numbers as strings
    assert_eq!(status["totalLength"].as_str(), Some("10000000"));
    assert_eq!(status["completedLength"].as_str(), Some("4200000"));
    assert_eq!(status["downloadSpeed"].as_str(), Some("512000"));
}

// =========================================================================
// Test 4: getGlobalStat aggregates live data from all groups
// =========================================================================

#[tokio::test]
async fn test_get_global_stat_aggregates_live_data() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Initially no downloads — stats should be zero
    let req = JsonRpcRequest::new("aria2.getGlobalStat", json!([])).with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let stat = resp.result.unwrap();
    // Wire format: all numbers as strings
    assert_eq!(stat["downloadSpeed"].as_str(), Some("0"));
    assert_eq!(stat["numActive"].as_str(), Some("0"));

    // Add two downloads with different speeds
    let add1 = JsonRpcRequest::new("aria2.addUri", json!([["http://a.com/f1"]])).with_id(2);
    let add2 = JsonRpcRequest::new("aria2.addUri", json!([["http://b.com/f2"]])).with_id(3);
    let gid1: String =
        serde_json::from_value(engine.handle_request(&add1).await.result.unwrap()).unwrap();
    let gid2: String =
        serde_json::from_value(engine.handle_request(&add2).await.result.unwrap()).unwrap();

    // Set both to Active with different speeds
    {
        let man = &group_man;
        let g1 = man.group_by_hex(&gid1).unwrap();
        let g2 = man.group_by_hex(&gid2).unwrap();
        let mut rg1 = g1.write().unwrap();
        rg1.start().unwrap();
        rg1.set_download_speed_cached(300_000);
        let mut rg2 = g2.write().unwrap();
        rg2.start().unwrap();
        rg2.set_download_speed_cached(200_000);
    }

    // getGlobalStat should aggregate: 300k + 200k = 500k, 2 active
    let req = JsonRpcRequest::new("aria2.getGlobalStat", json!([])).with_id(4);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let stat = resp.result.unwrap();
    // Wire format: all numbers as strings
    assert_eq!(stat["downloadSpeed"].as_str(), Some("500000"));
    assert_eq!(stat["numActive"].as_str(), Some("2"));
    assert_eq!(stat["numWaiting"].as_str(), Some("0"));
}

// =========================================================================
// Test 5: tellActive lists downloads with Active/Waiting status
// =========================================================================

#[tokio::test]
async fn test_tell_active_lists_active_downloads() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Add two downloads
    let add1 = JsonRpcRequest::new("aria2.addUri", json!([["http://a.com/f1"]])).with_id(1);
    let add2 = JsonRpcRequest::new("aria2.addUri", json!([["http://b.com/f2"]])).with_id(2);
    let gid1: String =
        serde_json::from_value(engine.handle_request(&add1).await.result.unwrap()).unwrap();
    let _gid2: String =
        serde_json::from_value(engine.handle_request(&add2).await.result.unwrap()).unwrap();

    // Set first to Active, leave second as Waiting (default)
    {
        let man = &group_man;
        let g1 = man.group_by_hex(&gid1).unwrap();
        let mut rg1 = g1.write().unwrap();
        rg1.start().unwrap();
    }

    // tellActive follows aria2 semantics and includes only the active group.
    let req = JsonRpcRequest::new("aria2.tellActive", json!([])).with_id(3);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let active = resp.result.unwrap();
    let arr = active.as_array().expect("tellActive should return array");
    assert_eq!(arr.len(), 1, "tellActive should exclude the waiting group");

    // Verify GIDs are present
    let gids: Vec<&str> = arr.iter().map(|v| v["gid"].as_str().unwrap()).collect();
    assert!(gids.contains(&gid1.as_str()));
}

// =========================================================================
// Test 6: Progress changes are reflected on subsequent tellStatus calls
// =========================================================================

#[tokio::test]
async fn test_progress_changes_reflected_in_tell_status() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Add a download
    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        json!([["http://example.com/progressive.bin"]]),
    )
    .with_id(1);
    let resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Set initial progress
    {
        let man = &group_man;
        let g = man.group_by_hex(&gid).unwrap();
        let mut rg = g.write().unwrap();
        rg.start().unwrap();
        rg.set_total_length_atomic(1_000_000);
        rg.set_completed_length(100_000);
        rg.set_download_speed_cached(50_000);
    }

    // First poll: 10% done
    let tell1 = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(2);
    let resp = engine.handle_request(&tell1).await;
    let s1 = resp.result.unwrap();
    // Wire format: all numbers as strings
    assert_eq!(s1["completedLength"].as_str(), Some("100000"));
    assert_eq!(s1["totalLength"].as_str(), Some("1000000"));

    // Simulate more progress
    {
        let man = &group_man;
        let g = man.group_by_hex(&gid).unwrap();
        let rg = g.read().unwrap(); // atomic setters only need &self
        rg.set_completed_length(500_000);
        rg.set_download_speed_cached(120_000);
    }

    // Second poll: 50% done, speed increased
    let tell2 = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(3);
    let resp = engine.handle_request(&tell2).await;
    let s2 = resp.result.unwrap();
    // Wire format: all numbers as strings
    assert_eq!(
        s2["completedLength"].as_str(),
        Some("500000"),
        "progress should update"
    );
    assert_eq!(
        s2["downloadSpeed"].as_str(),
        Some("120000"),
        "speed should update"
    );
}

// =========================================================================
// Test 7: aria2.getOption returns the option snapshot captured for the task.
// =========================================================================

#[tokio::test]
async fn test_get_option_preserves_task_snapshot_after_global_change() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // addUri captures the current global option set for this RequestGroup.
    let add_req = JsonRpcRequest::new("aria2.addUri", json!([["http://example.com/fallback.bin"]]))
        .with_id(1);
    let resp = engine.handle_request(&add_req).await;
    assert!(resp.is_success());
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Sanity check: the task really is in RequestGroupMan.
    assert!(
        group_man.group_by_hex(&gid).is_some(),
        "task should be registered in RequestGroupMan"
    );

    // Changing a global option updates future tasks, but must not rewrite the
    // option snapshot held by this existing group.
    let change_global = JsonRpcRequest::new(
        "aria2.changeGlobalOption",
        json!([{"dir": "/tmp/fallback-test"}]),
    )
    .with_id(2);
    let cg_resp = engine.handle_request(&change_global).await;
    assert!(cg_resp.is_success(), "changeGlobalOption should succeed");

    let get_global = JsonRpcRequest::new("aria2.getGlobalOption", json!([])).with_id(3);
    let global_resp = engine.handle_request(&get_global).await;
    assert_eq!(
        global_resp.result.unwrap()["dir"],
        "/tmp/fallback-test",
        "the global option should have changed"
    );

    // C++ GetOptionRpcMethod serializes group->getOption(), so this task
    // keeps its creation-time directory even though the global value changed.
    let get_req = JsonRpcRequest::new("aria2.getOption", json!([gid.clone()])).with_id(4);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(
        get_resp.is_success(),
        "getOption should return the task option snapshot"
    );

    let opts = get_resp.result.unwrap();
    let opts_map = opts
        .as_object()
        .expect("getOption result should be a JSON object");
    assert!(
        opts_map.contains_key("dir"),
        "task option snapshot should contain 'dir'"
    );
    assert_eq!(
        opts_map["dir"], ".",
        "getOption should not reflect a later changeGlobalOption call"
    );
    assert!(
        !opts_map.contains_key("enable-rpc"),
        "getOption must not expose process-wide RPC listener settings"
    );
}

// =========================================================================
// Test 8: aria2.getOption combines a task snapshot with applied changes.
// =========================================================================

#[tokio::test]
async fn test_get_option_merges_task_snapshot_with_applied_runtime_change() {
    let (engine, _group_man, _cmd_rx) = create_engine_with_shared_state();

    let add_req = JsonRpcRequest::new(
        "aria2.addUri",
        json!([
            ["http://example.com/runtime-change.bin"],
            {"dir": "/tmp/task-snapshot", "max-download-limit": "1024"}
        ]),
    )
    .with_id(1);
    let add_resp = engine.handle_request(&add_req).await;
    assert!(add_resp.is_success());
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    let change_req = JsonRpcRequest::new(
        "aria2.changeOption",
        json!([gid.clone(), {"max-download-limit": "2048"}]),
    )
    .with_id(2);
    let change_resp = engine.handle_request(&change_req).await;
    assert!(change_resp.is_success(), "changeOption should succeed");

    let get_req = JsonRpcRequest::new("aria2.getOption", json!([gid])).with_id(3);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
    let opts = get_resp.result.unwrap();

    assert_eq!(
        opts["dir"], "/tmp/task-snapshot",
        "getOption must retain unchanged fields from the task snapshot"
    );
    assert_eq!(
        opts["max-download-limit"], "2048",
        "getOption must expose the value applied by changeOption"
    );
}

// =========================================================================
// Test 9: aria2.getOption returns RpcExecution error for a GID that exists
// neither in RequestGroupMan nor in its stopped results.
// =========================================================================

#[tokio::test]
async fn test_get_option_errors_for_unknown_gid() {
    let (engine, _group_man, _cmd_rx) = create_engine_with_shared_state();

    let get_req = JsonRpcRequest::new("aria2.getOption", json!(["00000000000000ff"])).with_id(1);
    let resp = engine.handle_request(&get_req).await;
    assert!(
        resp.is_error(),
        "getOption for an unknown GID should return an error"
    );
    assert_eq!(
        resp.error.unwrap().code,
        1,
        "unknown GID should yield RpcExecution (1)"
    );
}
