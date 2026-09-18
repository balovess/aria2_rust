//! RPC HTTP server: `RpcServer` struct, axum routes and the HTTP request handlers.

use std::sync::Arc;

use super::config::ServerConfig;
use super::http_handlers::{
    DropHttpConnection, RpcState, build_cors_layer, handle_jsonrpc, handle_jsonrpc_or_ws,
    handle_xmlrpc, http_auth_middleware,
};
use super::tls::{TlsConfig, TlsError};
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
    fn from_parts(
        config: ServerConfig,
        tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
        engine: Arc<RpcEngine>,
    ) -> Self {
        Self {
            config,
            tls_acceptor,
            engine,
        }
    }

    fn load_tls_acceptor(
        config: &ServerConfig,
    ) -> Result<Option<tokio_rustls::TlsAcceptor>, TlsError> {
        config
            .tls
            .as_ref()
            .map(|tls_config| {
                tls_config
                    .load_server_config()
                    .map(tokio_rustls::TlsAcceptor::from)
            })
            .transpose()
    }

    fn from_config_and_engine(
        config: ServerConfig,
        engine: Arc<RpcEngine>,
    ) -> Result<Self, TlsError> {
        let tls_acceptor = Self::load_tls_acceptor(&config)?;
        Ok(Self::from_parts(config, tls_acceptor, engine))
    }

    /// Create a new RPC server with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if TLS configuration is provided but fails to load.
    pub fn new(config: ServerConfig) -> Result<Self, TlsError> {
        let engine = if let Some(token) = config.auth.token.as_deref() {
            Arc::new(
                RpcEngine::new().with_auth_middleware(super::auth::RpcAuthMiddleware::new(token)),
            )
        } else {
            Arc::new(RpcEngine::new())
        };

        Self::from_config_and_engine(config, engine)
    }

    /// Create a new RPC server with a pre-configured shared engine.
    /// Use this when the caller has already set up `group_man` and `cmd_tx`
    /// on the engine (e.g., when wiring to a running DownloadEngine).
    pub fn new_with_engine(config: ServerConfig, engine: Arc<RpcEngine>) -> Result<Self, TlsError> {
        Self::from_config_and_engine(config, engine)
    }

    /// Create a new HTTP RPC server (no TLS).
    pub fn new_http(host: impl Into<String>, port: u16) -> Self {
        Self::from_parts(
            ServerConfig::default().with_host(host).with_port(port),
            None,
            Arc::new(RpcEngine::new()),
        )
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
        let config = ServerConfig::default()
            .with_host(host)
            .with_port(port)
            .with_tls(tls_config);
        Self::from_config_and_engine(config, Arc::new(RpcEngine::new()))
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

    /// Bind the configured address before starting the serving task.
    ///
    /// Callers that own an application lifecycle can use this seam to report
    /// an occupied port synchronously instead of keeping the process alive
    /// with a background task that failed during startup.
    pub async fn bind_listener(
        &self,
    ) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error + Send + Sync>> {
        self.bind_listener_on(&self.config.host).await
    }

    /// Bind on a specific host while reusing this server's shared engine.
    pub async fn bind_listener_on(
        &self,
        host: &str,
    ) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error + Send + Sync>> {
        use std::net::SocketAddr;

        let addr: SocketAddr = if host.contains(':') {
            format!("[{host}]:{}", self.config.port).parse()?
        } else {
            format!("{host}:{}", self.config.port).parse()?
        };
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("RPC server listening on {}://{}", self.scheme(), addr);
        Ok(listener)
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
        let listener = self.bind_listener().await?;
        self.serve_on_listener(listener).await
    }

    /// Serve requests on a listener that was bound by the caller.
    ///
    /// This keeps listener ownership separate from router construction so an
    /// application can complete its startup handshake before spawning the
    /// long-lived server task.
    pub async fn serve_on_listener(
        &self,
        listener: tokio::net::TcpListener,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use axum::{
            Router, middleware,
            routing::{get, post},
        };

        // Create shared state with the persistent RPC engine
        let state = RpcState {
            engine: self.engine.clone(),
            max_request_size: self.config.max_request_size,
            auth: self.config.auth.clone(),
        };

        // Build the configured CORS layer once. The layer is immutable and
        // shared by all connections, while origin matching remains per request.
        let cors_layer = build_cors_layer(&self.config.cors);

        // Build router
        let app = Router::new()
            .route("/jsonrpc", post(handle_jsonrpc))
            .route("/jsonrpc", get(handle_jsonrpc_or_ws)) // GET + WebSocket upgrade
            .route("/rpc", post(handle_xmlrpc))
            .layer(axum::extract::DefaultBodyLimit::max(
                self.config.max_request_size,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                http_auth_middleware,
            ))
            .layer(cors_layer)
            .with_state(state);

        // Serve with or without TLS
        if let Some(ref tls_acceptor) = self.tls_acceptor {
            // HTTPS mode — accept TCP connections, perform TLS handshake,
            // then hand the encrypted stream to hyper/axum.
            tracing::info!("TLS enabled, serving HTTPS");
            self.serve_tls(listener, tls_acceptor.clone(), app).await?;
        } else {
            self.serve_http(listener, app).await?;
        }

        Ok(())
    }

    /// Serve cleartext HTTP with the same response-discard path as HTTPS.
    ///
    /// aria2_original closes a connection without writing an HTTP response
    /// when an authenticated non-WebSocket request advertises an oversized
    /// Content-Length. Axum's high-level server requires an infallible
    /// service, so it cannot represent that transport result directly.
    async fn serve_http(
        &self,
        listener: tokio::net::TcpListener,
        app: axum::Router,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use axum::extract::Request;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use std::net::SocketAddr;
        use tower_service::Service;

        loop {
            let (connection, remote_addr) = listener.accept().await?;
            let mut make_service = app
                .clone()
                .into_make_service_with_connect_info::<SocketAddr>();

            tokio::spawn(async move {
                let router = make_service.call(remote_addr).await.unwrap();
                let io = TokioIo::new(connection);
                let hyper_service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let mut router = router.clone();
                        async move {
                            let response = router.call(request).await.unwrap();
                            if response.extensions().get::<DropHttpConnection>().is_some() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionAborted,
                                    "oversized aria2 RPC request",
                                ));
                            }
                            Ok::<_, std::io::Error>(response)
                        }
                    });

                if let Err(error) =
                    hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, hyper_service)
                        .await
                {
                    tracing::debug!(%remote_addr, %error, "HTTP connection ended");
                }
            });
        }
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

                // Build a hyper Service that delegates to the Router. The
                // response marker tells the connection layer to close without
                // writing bytes for aria2's oversized-request path.
                let hyper_service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let mut router = router.clone();
                        async move {
                            let response = router.call(request).await.unwrap();
                            if response.extensions().get::<DropHttpConnection>().is_some() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionAborted,
                                    "oversized aria2 RPC request",
                                ));
                            }
                            Ok::<_, std::io::Error>(response)
                        }
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

impl std::fmt::Debug for RpcServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcServer")
            .field("addr", &self.addr())
            .field("secure", &self.is_secure())
            .field("config", &self.config)
            .finish()
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
