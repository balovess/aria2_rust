//! Test helper for RPC E2E tests.
//!
//! Provides utilities to start an RPC server on a random port
//! and interact with it via HTTP and WebSocket clients.
//!
//! ## Crypto provider
//!
//! Because `reqwest` across the workspace uses `rustls-no-provider` (to
//! avoid `aws-lc-rs` on Windows), we install the `ring` crypto provider
//! once at the start of every E2E test binary.

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

use aria2_rpc::engine::RpcEngine;
use aria2_rpc::server::{RpcAuthMiddleware, RpcServer, ServerConfig};

/// Install the `ring` crypto provider for rustls.
///
/// Must be called before any `reqwest::Client` is constructed when reqwest
/// is built with `rustls-no-provider`.
///
/// Note: `install_default()` returns Err if a provider is already installed
/// (e.g., by another module's initializer). That is fine — we only need to
/// ensure a provider is present, not that we installed it.
fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Start an RPC HTTP server on a random available port for testing.
///
/// If `token` is `Some(...)`, the server is started with token-based
/// authentication.  If `None`, no authentication is required.
///
/// Returns `(base_url, server_handle)`.  The server runs in a background
/// tokio task; dropping `server_handle` aborts it.
pub async fn start_test_server(token: Option<&str>) -> (String, TestServerHandle) {
    // Install the ring crypto provider before any reqwest::Client is built.
    ensure_crypto_provider();
    // Find a random available port via a pre-bind probe.
    let listener =
        StdTcpListener::bind("127.0.0.1:0").expect("Failed to bind to random port");
    let port = listener.local_addr().unwrap().port();
    drop(listener); // release so the RPC server can claim it

    let config = ServerConfig::default()
        .with_host("127.0.0.1")
        .with_port(port);

    // Wire auth onto the engine (ServerConfig.auth is decorative only;
    // the actual check lives in RpcEngine::auth_middleware).
    let engine = if let Some(t) = token {
        Arc::new(RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new(t)))
    } else {
        Arc::new(RpcEngine::new())
    };

    let server = RpcServer::new_with_engine(config, engine)
        .expect("Failed to create RpcServer");
    let base_url = format!("http://127.0.0.1:{}", port);

    let handle = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("[test-helper] RPC server exited with error: {e}");
        }
    });

    // Poll the root endpoint until the server is ready (≤ 2 s).
    wait_for_server_ready(&base_url).await;

    (base_url, TestServerHandle { inner: Some(handle) })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async fn wait_for_server_ready(base_url: &str) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_err = String::new();

    while tokio::time::Instant::now() < deadline {
        match client.get(base_url).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => {
                last_err = format!("status={}", resp.status());
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "Server at {base_url} did not become ready within 5 s (last error: {last_err})"
    );
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// RAII guard that aborts the background server on drop.
pub struct TestServerHandle {
    inner: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(h) = self.inner.take() {
            h.abort();
        }
    }
}
