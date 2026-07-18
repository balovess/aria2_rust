//! Integration test: RPC HTTP header passing and live progress feedback.
//!
//! Tests the three RPC fixes:
//! 1. `aria2.addUri` with `header`/`user-agent`/`referer` options → headers
//!    correctly stored in the `RequestGroup`'s `DownloadOptions`.
//! 2. `aria2.tellStatus` reads live progress (total/completed/speed) from
//!    `RequestGroupMan` atomic fields — not placeholder data.
//! 3. `aria2.getGlobalStat` / `aria2.tellActive` aggregate live data from
//!    all registered groups.

use aria2_core::engine::command::Command;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};

// =========================================================================
// Test Helpers
// =========================================================================

/// Create an `RpcEngine` wired to a real `RequestGroupMan` + command channel,
/// simulating the shared-state setup that the app uses in RPC mode.
fn create_engine_with_shared_state() -> (
    RpcEngine,
    Arc<RwLock<RequestGroupMan>>,
    mpsc::UnboundedReceiver<Box<dyn Command>>,
) {
    let group_man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Box<dyn Command>>();
    let engine = RpcEngine::new()
        .with_group_man(group_man.clone())
        .with_cmd_tx(cmd_tx);
    (engine, group_man, cmd_rx)
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

    // Verify a DownloadCommand was dispatched to the engine channel
    let cmd = tokio::time::timeout(std::time::Duration::from_millis(500), cmd_rx.recv())
        .await
        .expect("DownloadCommand should have been sent to cmd_tx")
        .expect("channel should not be closed");
    drop(cmd); // We don't execute it; just verifying it was sent

    // Verify the RequestGroup has the correct options stored
    let man = group_man.read().await;
    let group_lock = man
        .group_by_hex(&gid)
        .expect("group should be registered in RequestGroupMan");
    let g = group_lock.read().await;
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

    let man = group_man.read().await;
    let group_lock = man.group_by_hex(&gid).expect("group should exist");
    let g = group_lock.read().await;
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
    let add_req =
        JsonRpcRequest::new("aria2.addUri", json!(["http://example.com/largefile.bin"])).with_id(1);
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
        let man = group_man.read().await;
        let group_lock = man.group_by_hex(&gid).unwrap();
        let mut g = group_lock.write().await;
        g.start().await.unwrap(); // Status → Active
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
    // Per original aria2 protocol, all numeric fields are JSON strings.
    assert_eq!(status["totalLength"], "10000000");
    assert_eq!(status["completedLength"], "4200000");
    assert_eq!(status["downloadSpeed"], "512000");
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
    // Per original aria2 protocol, all GlobalStat fields are JSON strings
    // (util::itos / util::uitos). See RpcMethodImpl.cc:1382-1394.
    assert_eq!(stat["downloadSpeed"], "0");
    assert_eq!(stat["numActive"], "0");

    // Add two downloads with different speeds
    let add1 = JsonRpcRequest::new("aria2.addUri", json!(["http://a.com/f1"])).with_id(2);
    let add2 = JsonRpcRequest::new("aria2.addUri", json!(["http://b.com/f2"])).with_id(3);
    let gid1: String =
        serde_json::from_value(engine.handle_request(&add1).await.result.unwrap()).unwrap();
    let gid2: String =
        serde_json::from_value(engine.handle_request(&add2).await.result.unwrap()).unwrap();

    // Set both to Active with different speeds
    {
        let man = group_man.read().await;
        let g1 = man.group_by_hex(&gid1).unwrap();
        let g2 = man.group_by_hex(&gid2).unwrap();
        let mut rg1 = g1.write().await;
        rg1.start().await.unwrap();
        rg1.set_download_speed_cached(300_000);
        let mut rg2 = g2.write().await;
        rg2.start().await.unwrap();
        rg2.set_download_speed_cached(200_000);
    }

    // getGlobalStat should aggregate: 300k + 200k = 500k, 2 active
    let req = JsonRpcRequest::new("aria2.getGlobalStat", json!([])).with_id(4);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let stat = resp.result.unwrap();
    // All values are JSON strings (matches util::itos / util::uitos).
    assert_eq!(stat["downloadSpeed"], "500000");
    assert_eq!(stat["numActive"], "2");
    assert_eq!(stat["numWaiting"], "0");
}

// =========================================================================
// Test 5: tellActive lists downloads with Active/Waiting status
// =========================================================================

#[tokio::test]
async fn test_tell_active_lists_active_downloads() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Add two downloads
    let add1 = JsonRpcRequest::new("aria2.addUri", json!(["http://a.com/f1"])).with_id(1);
    let add2 = JsonRpcRequest::new("aria2.addUri", json!(["http://b.com/f2"])).with_id(2);
    let gid1: String =
        serde_json::from_value(engine.handle_request(&add1).await.result.unwrap()).unwrap();
    let _gid2: String =
        serde_json::from_value(engine.handle_request(&add2).await.result.unwrap()).unwrap();

    // Set first to Active, leave second as Waiting (default)
    {
        let man = group_man.read().await;
        let g1 = man.group_by_hex(&gid1).unwrap();
        let mut rg1 = g1.write().await;
        rg1.start().await.unwrap();
    }

    // tellActive should include both (is_active() returns true for Active AND Waiting)
    let req = JsonRpcRequest::new("aria2.tellActive", json!([])).with_id(3);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    let active = resp.result.unwrap();
    let arr = active.as_array().expect("tellActive should return array");
    assert_eq!(arr.len(), 2, "both downloads should be active/waiting");

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
        json!(["http://example.com/progressive.bin"]),
    )
    .with_id(1);
    let resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Set initial progress
    {
        let man = group_man.read().await;
        let g = man.group_by_hex(&gid).unwrap();
        let mut rg = g.write().await;
        rg.start().await.unwrap();
        rg.set_total_length_atomic(1_000_000);
        rg.set_completed_length(100_000);
        rg.set_download_speed_cached(50_000);
    }

    // First poll: 10% done
    let tell1 = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(2);
    let resp = engine.handle_request(&tell1).await;
    let s1 = resp.result.unwrap();
    // Per original aria2 protocol, numeric fields are JSON strings.
    assert_eq!(s1["completedLength"], "100000");
    assert_eq!(s1["totalLength"], "1000000");

    // Simulate more progress
    {
        let man = group_man.read().await;
        let g = man.group_by_hex(&gid).unwrap();
        let rg = g.read().await; // atomic setters only need &self
        rg.set_completed_length(500_000);
        rg.set_download_speed_cached(120_000);
    }

    // Second poll: 50% done, speed increased
    let tell2 = JsonRpcRequest::new("aria2.tellStatus", json!([gid.clone()])).with_id(3);
    let resp = engine.handle_request(&tell2).await;
    let s2 = resp.result.unwrap();
    assert_eq!(s2["completedLength"], "500000", "progress should update");
    assert_eq!(s2["downloadSpeed"], "120000", "speed should update");
}

// =========================================================================
// Test 7: aria2.getOption falls back to global options for tasks that exist
// in RequestGroupMan but have no per-task overrides stored via changeOption.
// =========================================================================

#[tokio::test]
async fn test_get_option_falls_back_to_global_for_group_man_task() {
    let (engine, group_man, _cmd_rx) = create_engine_with_shared_state();

    // Add a download — this registers the GID in RequestGroupMan but does
    // NOT create a task_opts entry (changeOption is never called).
    let add_req =
        JsonRpcRequest::new("aria2.addUri", json!(["http://example.com/fallback.bin"])).with_id(1);
    let resp = engine.handle_request(&add_req).await;
    assert!(resp.is_success());
    let gid: String = serde_json::from_value(resp.result.unwrap()).unwrap();

    // Sanity check: the task really is in RequestGroupMan.
    assert!(
        group_man.read().await.group_by_hex(&gid).is_some(),
        "task should be registered in RequestGroupMan"
    );

    // Set a known global option value so we can verify the fallback returns
    // the live global options (not a stale snapshot or an error).
    let change_global = JsonRpcRequest::new(
        "aria2.changeGlobalOption",
        json!([{"dir": "/tmp/fallback-test"}]),
    )
    .with_id(2);
    let cg_resp = engine.handle_request(&change_global).await;
    assert!(cg_resp.is_success(), "changeGlobalOption should succeed");

    // getOption on the task (no per-task overrides) should succeed and
    // return the global options, including the value we just set.
    let get_req = JsonRpcRequest::new("aria2.getOption", json!([gid.clone()])).with_id(3);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(
        get_resp.is_success(),
        "getOption should fall back to global options, not error"
    );

    let opts = get_resp.result.unwrap();
    let opts_map = opts
        .as_object()
        .expect("getOption result should be a JSON object");
    assert!(
        opts_map.contains_key("dir"),
        "fallback result should contain global 'dir' option"
    );
    assert_eq!(
        opts_map["dir"], "/tmp/fallback-test",
        "fallback should reflect the current global option value"
    );
}

// =========================================================================
// Test 8: aria2.getOption returns MethodNotFound for a GID that exists
// neither in task_opts nor in RequestGroupMan.
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
        -32601,
        "unknown GID should yield MethodNotFound (-32601)"
    );
}
