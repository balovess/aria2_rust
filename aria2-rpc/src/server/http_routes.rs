//! RPC HTTP server: `RpcServer` struct, axum routes and the HTTP request handlers.

use std::sync::Arc;

use super::auth::AuthConfig;
use super::config::ServerConfig;
use super::cors::CorsConfig;
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

        let engine = if let Some(token) = config.auth.token.as_deref() {
            Arc::new(
                RpcEngine::new().with_auth_middleware(super::auth::RpcAuthMiddleware::new(token)),
            )
        } else {
            Arc::new(RpcEngine::new())
        };

        Ok(Self {
            config,
            tls_acceptor,
            engine,
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
            Router, middleware,
            routing::{get, post},
        };
        use std::net::SocketAddr;
        use tokio::net::TcpListener;

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
            .route("/ws", get(ws_handler)) // WebSocket upgrade (backward compat)
            .route("/", get(root_handler))
            .layer(axum::extract::DefaultBodyLimit::max(
                self.config.max_request_size,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                http_auth_middleware,
            ))
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
    /// HTTP Basic Auth configuration. Token auth remains in `RpcEngine` and
    /// is carried in the JSON/XML-RPC parameter contract.
    pub(crate) auth: AuthConfig,
    /// Maximum allowed size for a single WebSocket frame/message in bytes.
    pub(crate) max_request_size: usize,
}

/// Convert the public CORS configuration into tower-http's request-aware
/// layer. `AllowOrigin::list` mirrors an allowed origin back to the browser,
/// while wildcard mode retains aria2's literal `*` response.
fn build_cors_layer(config: &CorsConfig) -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderName, HeaderValue, Method};
    use std::time::Duration;
    use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

    let methods = if config.allow_methods.trim() == "*" {
        AllowMethods::any()
    } else {
        AllowMethods::list(
            config
                .allow_methods
                .split(',')
                .filter_map(|method| method.trim().parse::<Method>().ok()),
        )
    };
    let headers = if config.allow_headers.trim() == "*" {
        AllowHeaders::any()
    } else {
        AllowHeaders::list(
            config
                .allow_headers
                .split(',')
                .filter_map(|header| header.trim().parse::<HeaderName>().ok()),
        )
    };
    let max_age = aria2_core::constants::CORS_MAX_AGE
        .parse::<u64>()
        .unwrap_or_default();

    let origin = if config.is_wildcard() {
        if config.allow_credentials {
            AllowOrigin::mirror_request()
        } else {
            Any.into()
        }
    } else {
        AllowOrigin::list(
            config
                .allowed_origins()
                .iter()
                .filter_map(|origin| HeaderValue::from_str(origin).ok()),
        )
    };

    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods(methods)
        .allow_headers(headers)
        .allow_credentials(config.allow_credentials)
        .max_age(Duration::from_secs(max_age))
}

/// Enforce HTTP Basic Auth at the transport seam. `OPTIONS` is deliberately
/// exempt, matching aria2's CORS preflight behavior; RPC token auth still
/// applies after a request reaches the JSON/XML engine.
async fn http_auth_middleware(
    axum::extract::State(state): axum::extract::State<RpcState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{Method, StatusCode, header};
    use axum::response::IntoResponse;

    let authorized = request.method() == Method::OPTIONS
        || !state.auth.has_basic()
        || state.auth.verify_authorization(
            request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        );

    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"aria2\"")],
            "Unauthorized",
        )
            .into_response()
    }
}

struct JsonGetRequest {
    body: Vec<u8>,
    callback: Option<String>,
}

/// Parse aria2's legacy JSON-RPC GET/JSONP query format.
///
/// The original server accepts `method`, `id`, and a URL-encoded Base64
/// `params` value. When both `method` and `id` are omitted, the decoded params
/// are treated as a complete batch request. `jsoncallback` wraps the response
/// in a JavaScript callback.
fn parse_json_get_query(query: &str) -> Result<JsonGetRequest, crate::json_rpc::JsonRpcError> {
    use base64::Engine;
    use serde_json::{Map, Value};

    let mut method = None;
    let mut id = None;
    let mut params = None;
    let mut callback = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "method" => method = Some(value.into_owned()),
            "id" => id = Some(value.into_owned()),
            "params" => params = Some(value.into_owned()),
            "jsoncallback" => callback = Some(value.into_owned()),
            _ => {}
        }
    }

    let decoded_params = params
        .map(|encoded| {
            // Form decoding turns an unescaped '+' into a space. Tolerate it
            // for clients that omitted URL encoding around standard Base64.
            let encoded = encoded.replace(' ', "+");
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| {
                    crate::json_rpc::JsonRpcError::ParseError(format!(
                        "invalid GET params base64: {error}"
                    ))
                })
        })
        .transpose()?;

    let body = match (method, id) {
        (None, None) => decoded_params.ok_or_else(|| {
            crate::json_rpc::JsonRpcError::ParseError("GET params are missing".into())
        })?,
        (method, id) => {
            let mut request = Map::new();
            if let Some(method) = method {
                request.insert("method".into(), Value::String(method));
            }
            if let Some(id) = id {
                request.insert("id".into(), Value::String(id));
            }
            if let Some(params) = decoded_params {
                let params = serde_json::from_slice(&params).map_err(|error| {
                    crate::json_rpc::JsonRpcError::ParseError(format!(
                        "invalid GET params JSON: {error}"
                    ))
                })?;
                request.insert("params".into(), params);
            }
            serde_json::to_vec(&Value::Object(request)).map_err(|error| {
                crate::json_rpc::JsonRpcError::InternalError(format!(
                    "failed to build GET request: {error}"
                ))
            })?
        }
    };

    if let Some(callback) = callback.as_deref()
        && !is_valid_jsonp_callback(callback)
    {
        return Err(crate::json_rpc::JsonRpcError::InvalidRequest(
            "invalid jsoncallback".into(),
        ));
    }

    Ok(JsonGetRequest { body, callback })
}

fn is_valid_jsonp_callback(callback: &str) -> bool {
    callback.split('.').all(|segment| {
        let mut chars = segment.chars();
        matches!(chars.next(), Some('$' | '_' | 'a'..='z' | 'A'..='Z'))
            && chars.all(|ch| matches!(ch, '$' | '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
    }) && !callback.is_empty()
}

struct JsonRpcHttpResponse {
    status: axum::http::StatusCode,
    body: String,
}

fn http_status_for_jsonrpc_error(code: i32) -> axum::http::StatusCode {
    use axum::http::StatusCode;

    match code {
        // aria2 maps execution failures and malformed requests to 400. Keep
        // the standard MethodNotFound mapping available for callers that use
        // -32601, although aria2's own unknown-method path uses code 1.
        1 | -32600 => StatusCode::BAD_REQUEST,
        -32601 => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn serialize_jsonrpc_response(response: crate::json_rpc::JsonRpcResponse) -> JsonRpcHttpResponse {
    let status = response
        .error
        .as_ref()
        .map(|error| http_status_for_jsonrpc_error(error.code))
        .unwrap_or(axum::http::StatusCode::OK);
    let body = response.to_string().unwrap_or_else(|error| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32603,\"message\":{}}}}}",
            serde_json::Value::String(error.to_string())
        )
    });
    JsonRpcHttpResponse { status, body }
}

async fn dispatch_jsonrpc_body(engine: &RpcEngine, body: &[u8]) -> JsonRpcHttpResponse {
    use crate::json_rpc::{JsonRpcBatchResponse, parse_request};

    match parse_request(body) {
        Ok(requests) if requests.len() == 1 => {
            serialize_jsonrpc_response(engine.handle_request(&requests[0]).await)
        }
        Ok(requests) => {
            let mut responses = Vec::with_capacity(requests.len());
            for request in &requests {
                responses.push(engine.handle_request(request).await);
            }
            let body = JsonRpcBatchResponse(responses)
                .to_string()
                .unwrap_or_else(|error| {
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32603,\"message\":{}}}}}",
                        serde_json::Value::String(error.to_string())
                    )
                });
            // aria2 always returns HTTP 200 for a batch envelope, even when
            // individual entries contain RPC errors.
            JsonRpcHttpResponse {
                status: axum::http::StatusCode::OK,
                body,
            }
        }
        Err(error) => serialize_jsonrpc_response(error.into_response(None)),
    }
}

fn wrap_jsonp(body: String, callback: Option<&str>) -> String {
    match callback {
        Some(callback) => format!("{callback}({body})"),
        None => body,
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
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    use axum::http::header;
    use axum::response::IntoResponse;

    let response = dispatch_jsonrpc_body(&state.engine, &body).await;

    (
        response.status,
        [(header::CONTENT_TYPE, "application/json-rpc")],
        response.body,
    )
        .into_response()
}

/// Handle the original aria2 XML-RPC endpoint at `/rpc`.
async fn handle_xmlrpc(
    axum::extract::State(state): axum::extract::State<RpcState>,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    use crate::json_rpc::JsonRpcRequest;
    use crate::xml_rpc::{XmlRpcResponse, XmlRpcValue, parse_request};
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let (status, response) = match parse_request(&body) {
        Ok(request) => {
            let params = request
                .params
                .iter()
                .map(XmlRpcValue::to_json_value)
                .collect::<Result<Vec<_>, _>>();
            match params {
                Ok(params) => {
                    let json_request =
                        JsonRpcRequest::new(request.method_name, serde_json::Value::Array(params))
                            .with_id(serde_json::Value::String("xmlrpc".into()));
                    let json_response = state.engine.handle_request(&json_request).await;
                    let response = match json_response.result {
                        Some(result) => XmlRpcValue::from_json_value(result)
                            .map(XmlRpcResponse::single)
                            .unwrap_or_else(|error| {
                                XmlRpcResponse::fault(-32603, &error.to_string())
                            }),
                        None => {
                            let error = json_response
                                .error
                                .map(|error| (error.code, error.message))
                                .unwrap_or((-32603, "Missing RPC response".into()));
                            XmlRpcResponse::fault(error.0, &error.1)
                        }
                    };
                    (StatusCode::OK, response)
                }
                Err(error) => (
                    StatusCode::BAD_REQUEST,
                    XmlRpcResponse::fault(error.fault_code(), &error.fault_string()),
                ),
            }
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            XmlRpcResponse::fault(error.fault_code(), &error.fault_string()),
        ),
    };

    (
        status,
        [(header::CONTENT_TYPE, "text/xml")],
        response.to_xml(),
    )
        .into_response()
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
    query: axum::extract::RawQuery,
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
            if let Some(query) = query.0.filter(|query| !query.is_empty()) {
                let parsed = match parse_json_get_query(&query) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        let response = serialize_jsonrpc_response(error.into_response(None));
                        return (
                            response.status,
                            [(axum::http::header::CONTENT_TYPE, "application/json-rpc")],
                            response.body,
                        )
                            .into_response();
                    }
                };
                let response = dispatch_jsonrpc_body(&state.engine, &parsed.body).await;
                let content_type = if parsed.callback.is_some() {
                    "text/javascript"
                } else {
                    "application/json-rpc"
                };
                return (
                    response.status,
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    wrap_jsonp(response.body, parsed.callback.as_deref()),
                )
                    .into_response();
            }

            // Keep a small discovery response for the Rust server's no-query
            // health-check extension. Original aria2 uses query parameters
            // for GET JSON-RPC and JSONP requests.
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
