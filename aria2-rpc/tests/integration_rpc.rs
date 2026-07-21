use std::collections::HashMap;
use std::sync::Arc;

use aria2_core::request::request_group::DownloadOptions;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::server::{AuthConfig, ServerConfig};
use tokio::sync::RwLock;

fn make_add_req(id: &str, uri: &str) -> JsonRpcRequest {
    JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!([[uri]]),
        id: Some(serde_json::Value::String(id.into())),
    }
}

#[tokio::test]
async fn test_engine_add_uri_returns_response() {
    let engine = RpcEngine::new();
    let req = make_add_req("req-1", "http://example.com/file.zip");
    let resp = engine.handle_request(&req).await;
    let result_str = serde_json::to_string(&resp).unwrap_or_default();
    assert!(!result_str.is_empty());
}

#[tokio::test]
async fn test_engine_pause_unpause_no_panic() {
    let engine = RpcEngine::new();
    let _add_resp = engine
        .handle_request(&make_add_req("add", "http://example.com/test.bin"))
        .await;

    let pause_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.pause".into(),
        params: serde_json::json!(["gid-001"]),
        id: Some(serde_json::Value::String("pause".into())),
    };
    let _pause_resp = engine.handle_request(&pause_req).await;

    let unpause_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.unpause".into(),
        params: serde_json::json!(["gid-001"]),
        id: Some(serde_json::Value::String("unpause".into())),
    };
    let _unpause_resp = engine.handle_request(&unpause_req).await;
}

#[tokio::test]
async fn test_engine_remove_nonexistent_is_error() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.remove".into(),
        params: serde_json::json!(["nonexistent-gid"]),
        id: Some(serde_json::Value::String("rm".into())),
    };
    assert!(engine.handle_request(&req).await.is_error());
}

#[tokio::test]
async fn test_engine_get_version_succeeds() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.getVersion".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("ver".into())),
    };
    assert!(engine.handle_request(&req).await.is_success());
}

#[tokio::test]
async fn test_engine_get_global_stat_succeeds() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.getGlobalStat".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("stat".into())),
    };
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success());
    if let Some(result) = resp.result {
        let json = serde_json::to_string(&result).unwrap_or_default();
        assert!(json.contains("numActive"));
    }
}

#[tokio::test]
async fn test_server_config_default() {
    let config = ServerConfig::default();
    assert_eq!(config.port, 6800);
    assert!(!config.host.is_empty());
}

#[tokio::test]
async fn test_auth_config_token_verify() {
    let auth = AuthConfig::default().with_token("my-secret-token");
    assert!(auth.verify_token("my-secret-token"));
    assert!(!auth.verify_token("wrong-token"));
}

#[tokio::test]
async fn test_auth_config_basic_verify() {
    let auth = AuthConfig::default().with_basic_auth("admin", "password123");
    assert!(auth.verify_basic("YWRtaW46cGFzc3dvcmQxMjM="));
}

#[tokio::test]
async fn test_engine_tell_active_empty() {
    let engine = RpcEngine::new();
    let req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.tellActive".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("active".into())),
    };
    assert!(engine.handle_request(&req).await.is_success());
}

#[tokio::test]
async fn test_engine_multiple_adds() {
    let engine = RpcEngine::new();
    for i in 0..3 {
        let uri = format!("http://example.com/file{}.zip", i);
        let req = make_add_req(&format!("add-{}", i), &uri);
        let resp = engine.handle_request(&req).await;
        let result_str = serde_json::to_string(&resp).unwrap_or_default();
        assert!(!result_str.is_empty());
    }

    let tell_active = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.tellActive".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("active".into())),
    };
    let active_resp = engine.handle_request(&tell_active).await;
    assert!(
        !serde_json::to_string(&active_resp)
            .unwrap_or_default()
            .is_empty()
    );
}

// =========================================================================
// Force Pause Tests
// =========================================================================

#[tokio::test]
async fn test_force_pause() {
    let engine = RpcEngine::new();

    // Add a task
    let add_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.addUri".into(),
        params: serde_json::json!(["http://example.com/file"]),
        id: Some(serde_json::Value::String("add".into())),
    };
    let add_resp = engine.handle_request(&add_req).await;
    let gid: String = serde_json::from_value(add_resp.result.unwrap()).unwrap();

    // Force pause the task
    let force_pause_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.forcePause".into(),
        params: serde_json::json!([gid.clone()]),
        id: Some(serde_json::Value::String("forcePause".into())),
    };
    let force_pause_resp = engine.handle_request(&force_pause_req).await;
    assert!(force_pause_resp.is_success(), "forcePause should succeed");

    // Verify the result is "OK"
    let result: String = serde_json::from_value(force_pause_resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK", "forcePause should return 'OK'");

    // Verify the task status is Paused
    let status_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.tellStatus".into(),
        params: serde_json::json!([gid]),
        id: Some(serde_json::Value::String("status".into())),
    };
    let status_resp = engine.handle_request(&status_req).await;
    assert!(status_resp.is_success());

    let status_json = status_resp.result.unwrap();
    let status_str = status_json.get("status").unwrap().as_str().unwrap();
    assert_eq!(
        status_str, "paused",
        "Task status should be 'paused' after forcePause"
    );
}

#[tokio::test]
async fn test_force_pause_nonexistent_gid() {
    let engine = RpcEngine::new();

    // Force pause a non-existent GID
    let force_pause_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.forcePause".into(),
        params: serde_json::json!(["nonexistent-gid-12345"]),
        id: Some(serde_json::Value::String("forcePause".into())),
    };
    let force_pause_resp = engine.handle_request(&force_pause_req).await;
    assert!(
        force_pause_resp.is_error(),
        "forcePause should fail for non-existent GID"
    );
    assert_eq!(
        force_pause_resp.error.unwrap().code,
        -32601,
        "Error code should be MethodNotFound"
    );
}

#[tokio::test]
async fn test_force_pause_all() {
    let engine = RpcEngine::new();

    // Add multiple tasks
    for i in 0..3 {
        let add_req = JsonRpcRequest {
            version: Some("2.0".into()),
            method: "aria2.addUri".into(),
            params: serde_json::json!([format!("http://example.com/file{}", i)]),
            id: Some(serde_json::Value::String(format!("add-{}", i))),
        };
        engine.handle_request(&add_req).await;
    }

    // Verify tasks are active
    let tell_active_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.tellActive".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("active".into())),
    };
    let active_resp = engine.handle_request(&tell_active_req).await;
    let active_tasks: Vec<serde_json::Value> =
        serde_json::from_value(active_resp.result.unwrap()).unwrap();
    assert_eq!(active_tasks.len(), 3, "Should have 3 active tasks");

    // Force pause all
    let force_pause_all_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.forcePauseAll".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("forcePauseAll".into())),
    };
    let force_pause_all_resp = engine.handle_request(&force_pause_all_req).await;
    assert!(
        force_pause_all_resp.is_success(),
        "forcePauseAll should succeed"
    );

    // Verify result is "OK"
    let result: String = serde_json::from_value(force_pause_all_resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK", "forcePauseAll should return 'OK'");

    // Verify no active tasks remain
    let active_resp2 = engine.handle_request(&tell_active_req).await;
    let active_tasks2: Vec<serde_json::Value> =
        serde_json::from_value(active_resp2.result.unwrap()).unwrap();
    assert_eq!(
        active_tasks2.len(),
        0,
        "No tasks should be active after forcePauseAll"
    );
}

#[tokio::test]
async fn test_force_pause_all_empty_tasks() {
    let engine = RpcEngine::new();

    // Force pause all when no tasks exist
    let force_pause_all_req = JsonRpcRequest {
        version: Some("2.0".into()),
        method: "aria2.forcePauseAll".into(),
        params: serde_json::json!([]),
        id: Some(serde_json::Value::String("forcePauseAll".into())),
    };
    let force_pause_all_resp = engine.handle_request(&force_pause_all_req).await;
    assert!(
        force_pause_all_resp.is_success(),
        "forcePauseAll should succeed even with no tasks"
    );

    let result: String = serde_json::from_value(force_pause_all_resp.result.unwrap()).unwrap();
    assert_eq!(result, "OK");
}

// =========================================================================
// changeOption Runtime-Changeable Propagation Tests
// =========================================================================

/// Register a group in a fresh `RequestGroupMan` and return the hex GID
/// plus the shared manager. The group is registered directly (not via
/// `aria2.addUri`) so no download engine / command channel is required.
async fn setup_group_man_with_group() -> (Arc<RwLock<RequestGroupMan>>, String) {
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let gid = {
        let guard = man.read().await;
        guard
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .expect("add_group should succeed")
    };
    (man, gid.to_hex_string())
}

#[tokio::test]
async fn test_change_option_propagates_to_running_group() {
    let (man, gid_hex) = setup_group_man_with_group().await;
    let engine = RpcEngine::new().with_group_man(man.clone());

    // changeOption with a runtime-changeable key should succeed and propagate
    // to the running RequestGroup.
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid_hex.clone(), {"max-download-limit": 102400}]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(resp.is_success(), "changeOption should succeed");

    // getOption should return the new value from task_opts.
    let get_req =
        JsonRpcRequest::new("aria2.getOption", serde_json::json!([gid_hex.clone()])).with_id(2);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
    let opts: HashMap<String, serde_json::Value> =
        serde_json::from_value(get_resp.result.unwrap()).unwrap();
    // Wire format: numbers are strings
    assert_eq!(
        opts.get("max-download-limit")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        Some(102400),
        "getOption should reflect the new max-download-limit"
    );

    // The running RequestGroup should also have the updated option, proving
    // that update_group_options propagated the change to the live download.
    let gid = aria2_core::request::request_group::GroupId::from_hex_string(&gid_hex)
        .expect("GID hex should parse");
    let group = man
        .read()
        .await
        .group_by_id(gid)
        .expect("group should still exist");
    let g = group.read().unwrap();
    assert_eq!(
        g.options().max_download_limit,
        Some(102400),
        "running group should have the propagated max-download-limit"
    );
}

#[tokio::test]
async fn test_change_option_rejects_startup_only_with_group_man() {
    let (man, gid_hex) = setup_group_man_with_group().await;
    let engine = RpcEngine::new().with_group_man(man);

    // `dir` is a known option (in VALID_OPTION_KEYS) but startup-only, so
    // changeOption must reject it with InvalidParams even when a running
    // group exists.
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([gid_hex, {"dir": "/tmp/downloads"}]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_error(),
        "changeOption with a startup-only key should fail"
    );
    assert_eq!(
        resp.error.unwrap().code,
        -32602,
        "error code should be InvalidParams (-32602)"
    );
}

#[tokio::test]
async fn test_change_option_unknown_gid_stores_in_task_opts() {
    // When group_man is set but the GID is not registered (task not started
    // yet), changeOption should still succeed and store in task_opts so the
    // change applies when the task starts later.
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let engine = RpcEngine::new().with_group_man(man);

    let unknown_gid = "0000000000000001".to_string();
    let req = JsonRpcRequest::new(
        "aria2.changeOption",
        serde_json::json!([unknown_gid.clone(), {"max-retries": 7}]),
    )
    .with_id(1);
    let resp = engine.handle_request(&req).await;
    assert!(
        resp.is_success(),
        "changeOption for unregistered GID should still succeed (stored in task_opts)"
    );

    // getOption should return the stored value.
    let get_req =
        JsonRpcRequest::new("aria2.getOption", serde_json::json!([unknown_gid])).with_id(2);
    let get_resp = engine.handle_request(&get_req).await;
    assert!(get_resp.is_success());
    let opts: HashMap<String, serde_json::Value> =
        serde_json::from_value(get_resp.result.unwrap()).unwrap();
    // Wire format: all numbers are serialized as strings to match original aria2.
    let max_retries_val = opts.get("max-retries").unwrap();
    assert!(
        max_retries_val.is_string(),
        "max-retries should be a string in wire format"
    );
    assert_eq!(
        max_retries_val.as_str(),
        Some("7"),
        "getOption should return the stored max-retries"
    );
}
