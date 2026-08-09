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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use std::time::Duration;

use tokio::sync::RwLock;

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
    start_test_server_with_max_concurrent(token, 5).await
}

/// Start an RPC server with an explicit promotion limit for queue-state tests.
/// A zero limit leaves new groups in the reserved queue.
pub async fn start_test_server_with_max_concurrent(
    token: Option<&str>,
    max_concurrent: u32,
) -> (String, TestServerHandle) {
    start_test_server_with_config(token, max_concurrent, ServerConfig::default()).await
}

/// Start an RPC server with an explicit server configuration.
///
/// The helper still owns the download-engine fixture and reserves a fresh
/// port; the supplied configuration controls HTTP-facing behavior such as
/// Basic Auth and CORS.
#[allow(dead_code)]
pub async fn start_test_server_with_config(
    token: Option<&str>,
    max_concurrent: u32,
    config: ServerConfig,
) -> (String, TestServerHandle) {
    // Ensure the ring crypto provider is installed before constructing clients.
    ensure_crypto_provider();
    // Find a random available port via a pre-bind probe.
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("Failed to bind to random port");
    let port = listener
        .local_addr()
        .expect("Failed to read random listener address")
        .port();
    drop(listener); // release so the RPC server can claim it

    let config = config.with_host("127.0.0.1").with_port(port);

    let group_man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let mut download_engine = DownloadEngine::new(1);
    group_man.read().await.set_max_concurrent(max_concurrent);
    download_engine.set_request_group_man(Arc::clone(&group_man));
    download_engine.set_keep_alive(true);
    let engine_cmd_tx = download_engine.engine_command_sender();
    let shutdown_tx = download_engine
        .take_shutdown_sender()
        .expect("new download engine must have a shutdown sender");
    let engine_task = tokio::spawn(async move {
        if let Err(e) = download_engine.run().await {
            eprintln!("[test-helper] DownloadEngine exited with error: {e}");
        }
    });

    // Wire token auth onto the engine, matching aria2's token-in-params
    // contract. HTTP Basic Auth is enforced by RpcServer from ServerConfig.
    // Also configure a unique session path for each fixture.
    static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
    let session_id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let save_session_path = std::env::temp_dir().join(format!(
        "aria2_rpc_e2e_{}_{}.sess",
        std::process::id(),
        session_id
    ));
    let rpc_engine = RpcEngine::wired(Arc::clone(&group_man), engine_cmd_tx)
        .with_save_session_path(save_session_path);
    let rpc_engine = if let Some(t) = token {
        rpc_engine.with_auth_middleware(RpcAuthMiddleware::new(t))
    } else {
        rpc_engine
    };
    let server = RpcServer::new_with_engine(config, Arc::new(rpc_engine))
        .expect("Failed to create RpcServer");
    let base_url = format!("http://127.0.0.1:{}", port);

    let server_task = tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            eprintln!("[test-helper] RPC server exited with error: {e}");
        }
    });

    // Poll the root endpoint until the server is ready (≤ 2 s).
    wait_for_server_ready(&base_url).await;

    (
        base_url,
        TestServerHandle {
            server_task: Some(server_task),
            engine_task: Some(engine_task),
            shutdown_tx: Some(shutdown_tx),
        },
    )
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async fn wait_for_server_ready(base_url: &str) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_err = String::new();

    while tokio::time::Instant::now() < deadline {
        match client.get(format!("{base_url}/jsonrpc")).send().await {
            // The original JSONP endpoint reports a parse error for a GET
            // without query parameters. Any non-404 HTTP response proves the
            // compatible route is accepting connections.
            Ok(resp) if resp.status() != reqwest::StatusCode::NOT_FOUND => return,
            Ok(resp) => {
                last_err = format!("status={}", resp.status());
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Server at {base_url} did not become ready within 5 s (last error: {last_err})");
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// RAII guard that aborts the background server on drop.
pub struct TestServerHandle {
    server_task: Option<tokio::task::JoinHandle<()>>,
    engine_task: Option<tokio::task::JoinHandle<()>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Some(task) = self.engine_task.take() {
            task.abort();
        }
    }
}
