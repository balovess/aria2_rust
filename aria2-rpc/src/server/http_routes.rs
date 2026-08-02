//! RPC HTTP server: `RpcServer` struct, axum routes and the HTTP request handlers.

use std::sync::Arc;

use super::config::ServerConfig;
use super::tls::{TlsConfig, TlsError};
use super::ws_session::{handle_ws_socket, ws_handler};
use crate::engine::RpcEngine;

/// RPC HTTP server supporting both HTTP and HTTPS.
///
/// Provides a tokio-based async server that handles JSON-RPC requests
/// over HTTP or HTTPS (TLS) depending on configuration.
pub struct RpcServer {
    /// Server configuration (host, port, auth, CORS, TLS)
    config: ServerConfig,
    /// TLS acceptor (None for HTTP, Some for HTTPS)
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    /// Shared RPC engine that persists across all requests.
    /// Holds download task state, group manager, and command channel.
    engine: Arc<RpcEngine>,
}

impl RpcServer {
    /// Create a new RPC server with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if TLS configuration is provided but fails to load.
    pub fn new(config: ServerConfig) -> Result<Self, TlsError> {
        let tls_acceptor = if let Some(ref tls_config) = config.tls {
            let server_config = tls_config.load_server_config()?;
            Some(tokio_rustls::TlsAcceptor::from(server_config))
        } else {
            None
        };

        Ok(Self {
            config,
            tls_acceptor,
            engine: Arc::new(RpcEngine::new()),
        })
    }

    /// Create a new RPC server with a pre-configured shared engine.
    /// Use this when the caller has already set up `group_man` and `cmd_tx`
    /// on the engine (e.g., when wiring to a running DownloadEngine).
    pub fn new_with_engine(config: ServerConfig, engine: Arc<RpcEngine>) -> Result<Self, TlsError> {
        let tls_acceptor = if let Some(ref tls_config) = config.tls {
            let server_config = tls_config.load_server_config()?;
            Some(tokio_rustls::TlsAcceptor::from(server_config))
        } else {
            None
        };

        Ok(Self {
            config,
            tls_acceptor,
            engine,
        })
    }

    /// Create a new HTTP RPC server (no TLS).
    pub fn new_http(host: impl Into<String>, port: u16) -> Self {
        Self {
            config: ServerConfig::default().with_host(host).with_port(port),
            tls_acceptor: None,
            engine: Arc::new(RpcEngine::new()),
        }
    }

    /// Create a new HTTPS RPC server with TLS.
    ///
    /// # Errors
    ///
    /// Returns an error if TLS configuration fails to load.
    pub fn new_https(
        host: impl Into<String>,
        port: u16,
        cert_path: impl Into<String>,
        key_path: impl Into<String>,
    ) -> Result<Self, TlsError> {
        let tls_config = TlsConfig::new(cert_path, key_path);
        let server_config = tls_config.load_server_config()?;
        let tls_acceptor = tokio_rustls::TlsAcceptor::from(server_config);

        Ok(Self {
            config: ServerConfig::default()
                .with_host(host)
                .with_port(port)
                .with_tls(tls_config),
            tls_acceptor: Some(tls_acceptor),
            engine: Arc::new(RpcEngine::new()),
        })
    }

    /// Get the server address string.
    pub fn addr(&self) -> String {
        self.config.addr()
    }

    /// Get the server port.
    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// Check if the server is using HTTPS.
    pub fn is_secure(&self) -> bool {
        self.tls_acceptor.is_some()
    }

    /// Get the protocol scheme.
    pub fn scheme(&self) -> &'static str {
        self.config.scheme()
    }

    /// Get the full URL for the RPC endpoint.
    pub fn rpc_url(&self) -> String {
        format!(
            "{}://{}{}",
            self.scheme(),
            self.addr(),
            crate::constants::RPC_ENDPOINT_PATH
        )
    }

    /// Get a reference to the server configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Get a reference to the TLS acceptor (if configured).
    pub fn tls_acceptor(&self) -> Option<&tokio_rustls::TlsAcceptor> {
        self.tls_acceptor.as_ref()
    }

    /// Start the RPC HTTP server and serve requests.
    ///
    /// This method runs forever until the server is shut down.
    /// It handles JSON-RPC requests at `/jsonrpc` endpoint.
    ///
    /// # Features
    ///
    /// - HTTP or HTTPS (TLS) based on configuration
    /// - CORS support with configurable allowed origins
    /// - Token-based authentication
    /// - JSON-RPC 2.0 request handling
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_rpc::server::{RpcServer, ServerConfig};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let server = RpcServer::new_http("127.0.0.1", 6800);
    ///     server.serve().await;
    /// }
    /// ```
    pub async fn serve(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use axum::{
            Router,
            http::{Method, header},
            routing::{get, post},
        };
        use std::net::SocketAddr;
        use tokio::net::TcpListener;
        use tower_http::cors::{Any, CorsLayer};

        // Create shared state with the persistent RPC engine
        let state = RpcState {
            engine: self.engine.clone(),
            max_request_size: self.config.max_request_size,
        };

        // Build CORS layer
        let cors_layer = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

        // Build router
        let app = Router::new()
            .route("/jsonrpc", post(handle_jsonrpc))
            .route("/jsonrpc", get(handle_jsonrpc_or_ws)) // GET + WebSocket upgrade
            .route("/rpc", post(handle_jsonrpc))
            .route("/ws", get(ws_handler)) // WebSocket upgrade (backward compat)
            .route("/", get(root_handler))
            .layer(cors_layer)
            .with_state(state);

        // Parse address
        let addr: SocketAddr = self.addr().parse()?;
        tracing::info!("RPC server listening on {}://{}", self.scheme(), addr);

        // Bind TCP listener
        let listener = TcpListener::bind(addr).await?;

        // Serve with or without TLS
        if let Some(ref tls_acceptor) = self.tls_acceptor {
            // HTTPS mode — accept TCP connections, perform TLS handshake,
            // then hand the encrypted stream to hyper/axum.
            tracing::info!("TLS enabled, serving HTTPS");
            self.serve_tls(listener, tls_acceptor.clone(), app).await?;
        } else {
            // HTTP mode
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
        }

        Ok(())
    }

    /// Serve HTTPS by accepting TCP connections, wrapping each with TLS,
    /// then dispatching to the axum router via hyper's low-level connection API.
    ///
    /// This follows the official axum `low-level-rustls` example pattern:
    /// each incoming TCP connection is TLS-accepted, then handed to
    /// `hyper_util::server::conn::auto::Builder` which handles both
    /// HTTP/1.1 and HTTP/2 (h2) over the encrypted stream.
    async fn serve_tls(
        &self,
        listener: tokio::net::TcpListener,
        tls_acceptor: tokio_rustls::TlsAcceptor,
        app: axum::Router,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use axum::extract::Request;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use std::net::SocketAddr;
        use tower_service::Service;

        loop {
            let (cnx, remote_addr) = listener.accept().await?;
            let tls_acceptor = tls_acceptor.clone();

            // Convert the Router into a MakeService that provides
            // ConnectInfo<SocketAddr> to handlers.
            let mut make_service = app
                .clone()
                .into_make_service_with_connect_info::<SocketAddr>();

            tokio::spawn(async move {
                // Perform TLS handshake
                let Ok(tls_stream) = tls_acceptor.accept(cnx).await else {
                    tracing::error!("TLS handshake failed for connection from {}", remote_addr);
                    return;
                };

                // Call the MakeService to obtain a per-connection Router.
                // IntoMakeServiceWithConnectInfo never returns Err, so unwrap is safe.
                let router = make_service.call(remote_addr).await.unwrap();

                // Bridge tokio AsyncRead/AsyncWrite → hyper's IO traits
                let io = TokioIo::new(tls_stream);

                // Build a hyper Service that delegates to the Router
                let hyper_service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        router.clone().call(request)
                    });

                let result = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(io, hyper_service)
                    .await;

                if let Err(err) = result {
                    tracing::warn!("HTTPS connection error from {}: {}", remote_addr, err);
                }
            });
        }
    }
}

/// Shared state for RPC handlers
#[derive(Clone)]
pub struct RpcState {
    pub(crate) engine: Arc<RpcEngine>,
    /// Maximum allowed size for a single WebSocket frame/message in bytes.
    pub(crate) max_request_size: usize,
}

impl std::fmt::Debug for RpcServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcServer")
            .field("addr", &self.addr())
            .field("secure", &self.is_secure())
            .field("config", &self.config)
            .finish()
    }
}

/// Root handler - returns server info
async fn root_handler() -> impl axum::response::IntoResponse {
    use axum::http::StatusCode;
    use axum::response::Json;
    use serde_json::json;

    (
        StatusCode::OK,
        Json(json!({
            "name": crate::constants::RPC_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "endpoints": {
                "jsonrpc": crate::constants::RPC_ENDPOINT_PATH,
                "rpc": "/rpc",
                "ws": "/ws"
            }
        })),
    )
}

/// Handle JSON-RPC POST requests
async fn handle_jsonrpc(
    axum::extract::State(state): axum::extract::State<RpcState>,
    axum::Json(req): axum::Json<crate::json_rpc::JsonRpcRequest>,
) -> impl axum::response::IntoResponse {
    use axum::http::StatusCode;
    use axum::response::Json;

    // Process request using the shared persistent engine
    let response = state.engine.handle_request(&req).await;

    (StatusCode::OK, Json(response))
}

/// Handle GET requests at `/jsonrpc`.
///
/// Supports two modes:
/// 1. **WebSocket upgrade** — If the request has `Upgrade: websocket` headers,
///    the connection is upgraded to WebSocket for real-time download events.
/// 2. **Regular GET** — Returns an informational message.
///
/// This dual behavior is required because Aria2 Explorer initiates WebSocket
/// connections at `/jsonrpc` (not `/ws`), while other clients may use GET for
/// health checks or debugging.
async fn handle_jsonrpc_or_ws(
    axum::extract::State(state): axum::extract::State<RpcState>,
    ws: Option<axum::extract::ws::WebSocketUpgrade>,
) -> impl axum::response::IntoResponse {
    use axum::http::StatusCode;
    use axum::response::Json;
    use serde_json::json;

    use axum::response::IntoResponse;

    match ws {
        Some(upgrade) => {
            // WebSocket upgrade request from Aria2 Explorer or other clients.
            // Enforce max frame/message size to prevent OOM from oversized payloads.
            let max_size = state.max_request_size;
            upgrade
                .max_frame_size(max_size)
                .max_message_size(max_size)
                .on_upgrade(move |socket| handle_ws_socket(socket, state.engine.clone()))
        }
        None => {
            // Regular GET request
            (
                StatusCode::OK,
                Json(json!({
                    "error": "Use POST for JSON-RPC requests, or connect via WebSocket at /jsonrpc (ws://)"
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
