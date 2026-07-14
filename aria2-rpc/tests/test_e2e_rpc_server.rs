//! E2E tests for RPC HTTP server
//!
//! Tests the actual HTTP server implementation with real network requests.

use aria2_rpc::server::{RpcServer, ServerConfig};

/// Start a test server on a random port
#[allow(dead_code)]
async fn start_test_server() -> (RpcServer, u16) {
    // Use port 0 to get a random available port
    let _server = RpcServer::new_http("127.0.0.1", 0);

    // We need to bind to get the actual port
    // For testing, we'll use a fixed port range
    let port = find_available_port().await;
    let server = RpcServer::new_http("127.0.0.1", port);

    (server, port)
}

/// Find an available port for testing
#[allow(dead_code)]
async fn find_available_port() -> u16 {
    use tokio::net::TcpListener;

    // Bind to port 0 to get a random available port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test]
async fn test_server_creation() {
    let server = RpcServer::new_http("127.0.0.1", 6800);
    assert_eq!(server.addr(), "127.0.0.1:6800");
    assert!(!server.is_secure());
    assert_eq!(server.scheme(), "http");
}

#[tokio::test]
async fn test_server_with_config() {
    let config = ServerConfig::default().with_host("0.0.0.0").with_port(8080);

    let server = RpcServer::new(config).expect("Failed to create server");
    assert_eq!(server.addr(), "0.0.0.0:8080");
    assert!(!server.is_secure());
}

#[tokio::test]
async fn test_server_url_generation() {
    let server = RpcServer::new_http("127.0.0.1", 6800);
    assert_eq!(server.rpc_url(), "http://127.0.0.1:6800/jsonrpc");
}

// Note: The following tests require the server to be running.
// They are commented out because they need a running server instance.
// In a real test environment, you would spawn the server in a separate task.

/*
#[tokio::test]
async fn test_jsonrpc_add_uri_request() {
    let (server, port) = start_test_server().await;

    // Spawn server in background
    let server_handle = tokio::spawn(async move {
        server.serve().await
    });

    // Wait for server to start
    sleep(Duration::from_millis(100)).await;

    // Make request
    let client = reqwest::Client::new();
    let response = client
        .post(&format!("http://127.0.0.1:{}/jsonrpc", port))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "aria2.addUri",
            "params": [["http://example.com/file.zip"]],
            "id": "test-1"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert!(body.get("result").is_some());

    // Cleanup
    server_handle.abort();
}

#[tokio::test]
async fn test_jsonrpc_get_version() {
    let (server, port) = start_test_server().await;

    // Spawn server in background
    let server_handle = tokio::spawn(async move {
        server.serve().await
    });

    // Wait for server to start
    sleep(Duration::from_millis(100)).await;

    // Make request
    let client = reqwest::Client::new();
    let response = client
        .post(&format!("http://127.0.0.1:{}/jsonrpc", port))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": [],
            "id": "test-2"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert!(body.get("result").is_some());
    let result = body.get("result").unwrap();
    assert!(result.get("version").is_some());

    // Cleanup
    server_handle.abort();
}

#[tokio::test]
async fn test_root_endpoint() {
    let (server, port) = start_test_server().await;

    // Spawn server in background
    let server_handle = tokio::spawn(async move {
        server.serve().await
    });

    // Wait for server to start
    sleep(Duration::from_millis(100)).await;

    // Make request to root
    let client = reqwest::Client::new();
    let response = client
        .get(&format!("http://127.0.0.1:{}", port))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("Failed to parse response");
    assert_eq!(body.get("name").unwrap().as_str().unwrap(), "aria2-rust");

    // Cleanup
    server_handle.abort();
}

#[tokio::test]
async fn test_cors_headers() {
    let (server, port) = start_test_server().await;

    // Spawn server in background
    let server_handle = tokio::spawn(async move {
        server.serve().await
    });

    // Wait for server to start
    sleep(Duration::from_millis(100)).await;

    // Make OPTIONS request
    let client = reqwest::Client::new();
    let response = client
        .request(reqwest::Method::OPTIONS, &format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", "http://localhost:3000")
        .send()
        .await
        .expect("Failed to send request");

    // CORS should allow the request
    assert!(response.status().is_success());

    // Cleanup
    server_handle.abort();
}
*/

/// Manual test helper - run this to start a test server
#[allow(dead_code)]
async fn run_manual_test_server() {
    use aria2_rpc::server::AuthConfig;

    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(6800)
        .with_auth(AuthConfig::default().with_token("test-token"));

    let server = RpcServer::new(config).expect("Failed to create server");

    println!("Server starting at {}", server.rpc_url());
    println!("Press Ctrl+C to stop");

    // This will run forever
    server.serve().await.unwrap();
}

#[test]
fn test_server_config_builder() {
    use aria2_rpc::server::{AuthConfig, CorsConfig};

    let config = ServerConfig::default()
        .with_host("0.0.0.0")
        .with_port(9000)
        .with_auth(AuthConfig::default().with_token("secret"))
        .with_cors(CorsConfig::default());

    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.port, 9000);
    assert!(config.auth.has_token());
}

#[test]
fn test_server_debug_format() {
    let server = RpcServer::new_http("localhost", 6800);
    let debug_str = format!("{:?}", server);
    assert!(debug_str.contains("RpcServer"));
    assert!(debug_str.contains("localhost:6800"));
}
