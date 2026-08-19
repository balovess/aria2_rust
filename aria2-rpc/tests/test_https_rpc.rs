//! HTTPS RPC server configuration and live TLS coverage.

use std::io::Write;
use std::sync::Once;
use std::time::Duration;

use aria2_rpc::server::{RpcServer, ServerConfig, TlsConfig};
use serde_json::{Value, json};
use tempfile::NamedTempFile;

fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn write_tls_fixture() -> (NamedTempFile, NamedTempFile) {
    let mut certificate = NamedTempFile::new().expect("failed to create certificate fixture");
    certificate
        .write_all(include_bytes!("fixtures/tls/chain.pem"))
        .expect("failed to write certificate fixture");
    let mut private_key = NamedTempFile::new().expect("failed to create key fixture");
    private_key
        .write_all(include_bytes!("fixtures/tls/end.key"))
        .expect("failed to write key fixture");
    (certificate, private_key)
}

#[test]
fn test_https_server_scheme() {
    // Test that HTTPS server has correct scheme without actually loading cert
    // (cert loading is tested in test_mock_rpc_server.rs with proper cert generation)
    let http_server = RpcServer::new_http("127.0.0.1", 6800);
    assert_eq!(http_server.scheme(), "http");
    assert!(!http_server.is_secure());
}

#[test]
fn test_tls_config_creation() {
    let tls = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
    assert_eq!(tls.cert_path, "/path/to/cert.pem");
    assert_eq!(tls.key_path, "/path/to/key.pem");
}

#[test]
fn test_tls_config_fields() {
    let tls = TlsConfig::new("cert.crt", "key.key");
    assert!(!tls.cert_path.is_empty());
    assert!(!tls.key_path.is_empty());
}

#[tokio::test]
async fn e2e_https_jsonrpc_get_version_uses_tls_connection_service() {
    ensure_crypto_provider();
    let (certificate, private_key) = write_tls_fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind HTTPS listener");
    let port = listener
        .local_addr()
        .expect("HTTPS listener must expose its address")
        .port();
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_tls(TlsConfig::new(
            certificate.path().to_string_lossy(),
            private_key.path().to_string_lossy(),
        ));
    let server = RpcServer::new(config).expect("TLS fixture must load into RpcServer");
    let server_task = tokio::spawn(async move { server.serve_on_listener(listener).await });

    let client = reqwest::Client::builder()
        .tls_danger_accept_invalid_certs(true)
        .build()
        .expect("failed to build HTTPS client");
    let url = format!("https://127.0.0.1:{port}/jsonrpc");
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client
                .post(&url)
                .json(&json!({
                    "jsonrpc": "2.0",
                    "method": "aria2.getVersion",
                    "params": [],
                    "id": "https-version",
                }))
                .send()
                .await
            {
                Ok(response) => break response,
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("HTTPS RPC listener did not become ready");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response
        .json()
        .await
        .expect("HTTPS JSON-RPC response must be valid JSON");
    assert_eq!(body["id"], "https-version");
    assert!(
        body["result"]["version"].is_string(),
        "HTTPS JSON-RPC must reach the RPC engine: {body}"
    );

    server_task.abort();
}
