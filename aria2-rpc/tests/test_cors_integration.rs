//! End-to-end CORS integration tests for the RPC HTTP server.
//!
//! These tests spawn a real `RpcServer` on an ephemeral port and verify
//! that the `CorsLayer` built inside `serve()` actually enforces the
//! `ServerConfig.cors` configuration. Prior to the Task 2.7 fix, `serve()`
//! hardcoded `CorsLayer::new().allow_origin(Any)`, completely bypassing
//! `CorsConfig` — these tests guard against that regression.

use std::time::Duration;

use aria2_rpc::server::{CorsConfig, RpcServer, ServerConfig};

/// Wait for a TCP port to start accepting connections, with a short retry loop.
///
/// `tokio::net::TcpListener::bind("127.0.0.1:0")` is used to pick a free port,
/// but there is an inherent race between dropping that listener and the
/// `RpcServer::serve()` call rebinding it. We tolerate `EADDRINUSE`/connection
/// refused for up to ~1 second.
async fn wait_for_server(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
            Ok(_) => return,
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("server on port {} did not come up within 2s", port);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Pick a free ephemeral port by binding to `:0` and immediately dropping the
/// listener. There is a TOCTOU race here, but it is acceptable for tests.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Spawn `server.serve()` in the background.
fn spawn_server(server: RpcServer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = server.serve().await;
    })
}

/// Build a JSON-RPC request body that the server will accept regardless of
/// CORS (no auth secret configured).
fn rpc_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "aria2.getVersion",
        "params": []
    })
}

// =========================================================================
// Wildcard mode (`"*"`) — mirrors original aria2 `--rpc-allow-origin-all`
// =========================================================================

#[tokio::test]
async fn cors_wildcard_sends_allow_origin_star() {
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::default()); // default is ["*"]
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", "http://example.test")
        .json(&rpc_body())
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_success(),
        "wildcard CORS should not block POST: {}",
        resp.status()
    );
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("wildcard mode must send Access-Control-Allow-Origin");
    assert_eq!(
        allow_origin.to_str().unwrap(),
        "*",
        "wildcard mode must send `*` (mirrors --rpc-allow-origin-all)"
    );

    handle.abort();
}

// =========================================================================
// Specific origins mode — must enforce the allow-list
// =========================================================================

#[tokio::test]
async fn cors_specific_origin_allowed_is_echoed() {
    let port = free_port().await;
    let allowed = "http://localhost:3000".to_string();
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::with_allowed_origins(vec![allowed.clone()]));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", &allowed)
        .json(&rpc_body())
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_success(),
        "allowed origin POST should succeed: {}",
        resp.status()
    );
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("allowed origin must be echoed back");
    assert_eq!(
        allow_origin.to_str().unwrap(),
        allowed,
        "specific-origins mode must echo the request Origin"
    );

    handle.abort();
}

#[tokio::test]
async fn cors_disallowed_origin_gets_no_allow_origin_header() {
    // This is the regression test for the Task 2.7 bug: previously `serve()`
    // hardcoded `allow_origin(Any)`, so disallowed origins still received
    // `Access-Control-Allow-Origin: *`. The fix must reject them.
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::with_allowed_origins(vec![
            "http://localhost:3000".to_string(),
        ]));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/jsonrpc", port))
        // Attacker origin — not on the allow-list
        .header("Origin", "http://evil.example")
        .json(&rpc_body())
        .send()
        .await
        .expect("request failed");

    // The response should NOT contain an Access-Control-Allow-Origin header.
    // (CORS does not block the server-side request — it only withholds the
    // header so the browser refuses to expose the response to the cross-origin
    // caller.)
    assert!(
        resp.headers()
            .get("access-control-allow-origin")
            .is_none(),
        "disallowed origin must NOT receive Access-Control-Allow-Origin header (regression of Task 2.7)"
    );

    handle.abort();
}

#[tokio::test]
async fn cors_specific_origins_preflight_allowed() {
    let port = free_port().await;
    let allowed = "http://localhost:3000".to_string();
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::with_allowed_origins(vec![allowed.clone()]));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", &allowed)
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .expect("preflight failed");

    assert!(
        resp.status().is_success(),
        "preflight for allowed origin should succeed: {}",
        resp.status()
    );
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("preflight response must include Access-Control-Allow-Origin");
    assert_eq!(allow_origin.to_str().unwrap(), allowed);

    handle.abort();
}

#[tokio::test]
async fn cors_specific_origins_preflight_disallowed_blocked() {
    let port = free_port().await;
    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_cors(CorsConfig::with_allowed_origins(vec![
            "http://localhost:3000".to_string(),
        ]));
    let server = RpcServer::new(config).expect("Failed to create server");
    let handle = spawn_server(server);
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("http://127.0.0.1:{}/jsonrpc", port))
        .header("Origin", "http://evil.example")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .expect("preflight failed");

    // Per CORS spec, the browser enforces preflight by checking the response
    // for the `Access-Control-Allow-Origin` header. If absent (or non-matching),
    // the browser refuses to send the actual cross-origin request — regardless
    // of the HTTP status code returned by the server. tower_http 0.5 may
    // return 200 (default OPTIONS passthrough) but must NOT include CORS
    // headers when the origin is disallowed.
    assert!(
        resp.headers()
            .get("access-control-allow-origin")
            .is_none(),
        "disallowed preflight must NOT include Access-Control-Allow-Origin header (browser-side enforcement is what matters)"
    );

    handle.abort();
}
