use super::super::auth::AuthConfig;
use super::*;
use crate::backend::{BackendError, BackendRequest, BackendResponse, BackendResult, RpcBackend};
use async_trait::async_trait;
use std::sync::Mutex;

#[derive(Clone, Default)]
struct RecordingBackend {
    requests: Arc<Mutex<Vec<BackendRequest>>>,
}

#[async_trait]
impl RpcBackend for RecordingBackend {
    fn metadata(&self) -> crate::backend::BackendMetadata {
        crate::backend::BackendMetadata::base("test")
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        let response = match &request {
            BackendRequest::AddUri { .. } => BackendResponse::Gid("0123456789abcdef".into()),
            _ => {
                return Err(BackendError::Unsupported(format!(
                    "unexpected test request: {request:?}"
                )));
            }
        };
        self.requests
            .lock()
            .expect("test backend lock poisoned")
            .push(request);
        Ok(BackendResult::response(response))
    }
}

#[test]
fn test_rpc_server_new_http() {
    let server = RpcServer::new_http("127.0.0.1", 6800);
    assert_eq!(server.addr(), "127.0.0.1:6800");
    assert_eq!(server.port(), 6800);
    assert!(!server.is_secure());
    assert_eq!(server.scheme(), "http");
    assert_eq!(server.rpc_url(), "http://127.0.0.1:6800/jsonrpc");
}

#[test]
fn test_rpc_server_from_config() {
    let config = ServerConfig::default().with_host("0.0.0.0").with_port(8080);

    let server = RpcServer::new(config).expect("Failed to create server");
    assert_eq!(server.addr(), "0.0.0.0:8080");
    assert!(!server.is_secure());
}

#[test]
fn test_rpc_server_from_backend_preserves_server_configuration() {
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(6801)
        .with_auth(AuthConfig::default().with_token("secret"));

    let server = RpcServer::new_with_backend(config, Arc::new(crate::backend::UnsupportedBackend))
        .expect("backend-backed server should construct");

    assert_eq!(server.addr(), "127.0.0.1:6801");
    assert!(server.config().auth.has_token());
}

#[tokio::test]
async fn test_rpc_server_from_backend_dispatches_to_the_same_engine() {
    let backend = RecordingBackend::default();
    let requests = Arc::clone(&backend.requests);
    let server = RpcServer::new_with_backend(
        ServerConfig::default().with_auth(AuthConfig::default().with_token("secret")),
        Arc::new(backend),
    )
    .expect("backend-backed server should construct");

    let request = crate::json_rpc::JsonRpcRequest::new(
        "aria2.addUri",
        serde_json::json!(["token:secret", ["https://example.test/file"]]),
    )
    .with_id(1);
    let response = server.engine().handle_request(&request).await;

    assert_eq!(response.result, Some(serde_json::json!("0123456789abcdef")));
    assert!(response.error.is_none());
    assert!(matches!(
        requests.lock().expect("test backend lock poisoned").as_slice(),
        [BackendRequest::AddUri { uris, .. }] if uris == &["https://example.test/file"]
    ));
}

#[test]
fn test_rpc_server_debug_format() {
    let server = RpcServer::new_http("localhost", 6800);
    let debug_str = format!("{:?}", server);
    assert!(debug_str.contains("RpcServer"));
    assert!(debug_str.contains("localhost:6800"));
    assert!(debug_str.contains("secure: false"));
}

#[test]
fn test_rpc_server_config_accessor() {
    let config = ServerConfig::default()
        .with_host("192.168.1.1")
        .with_port(9999);

    let server = RpcServer::new(config).expect("Failed to create server");
    let cfg = server.config();
    assert_eq!(cfg.host, "192.168.1.1");
    assert_eq!(cfg.port, 9999);
}

#[test]
fn test_rpc_server_tls_acceptor_none_for_http() {
    let server = RpcServer::new_http("127.0.0.1", 6800);
    assert!(server.tls_acceptor().is_none());
}

#[tokio::test]
async fn test_rpc_server_stops_accepting_after_shutdown_signal() {
    let server = RpcServer::new_http("127.0.0.1", 0);
    let listener = server.bind_listener().await.expect("listener should bind");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        server
            .serve_on_listener_with_shutdown(listener, async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    shutdown_tx
        .send(())
        .expect("shutdown receiver should exist");
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("server should stop after shutdown")
        .expect("server task should not panic");
    assert!(result.is_ok());
}
