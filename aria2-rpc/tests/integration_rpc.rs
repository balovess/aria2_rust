use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::server::{AuthConfig, ServerConfig};

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
