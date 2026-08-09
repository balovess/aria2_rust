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

    /// Bind the configured address before starting the serving task.
    ///
    /// Callers that own an application lifecycle can use this seam to report
    /// an occupied port synchronously instead of keeping the process alive
    /// with a background task that failed during startup.
    pub async fn bind_listener(
        &self,
    ) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error + Send + Sync>> {
        use std::net::SocketAddr;

        let addr: SocketAddr = self.addr().parse()?;
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
        use std::net::SocketAddr;

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
/// The C++ implementation intentionally has a small, legacy grammar here:
/// it matches raw `key=value` prefixes, percent-decodes only `params`, and
/// copies `method`, `id`, and `jsoncallback` into the generated request or
/// response without form normalization. Keep those rules at this wire seam;
/// the POST JSON parser must not inherit them.
fn parse_json_get_query(query: &str) -> JsonGetRequest {
    let query = query.strip_prefix('?').unwrap_or(query);
    let mut method = None;
    let mut id = None;
    let mut params = None;
    let mut callback = None;

    for item in query.split('&') {
        if let Some(value) = item.strip_prefix("method=") {
            method = Some(value);
        } else if let Some(value) = item.strip_prefix("id=") {
            id = Some(value);
        } else if let Some(value) = item.strip_prefix("params=") {
            params = Some(value);
        } else if let Some(value) = item.strip_prefix("jsoncallback=") {
            callback = Some(value);
        }
    }

    let has_params = params.is_some_and(|encoded| !encoded.is_empty());
    let decoded_params = params.map(|encoded| {
        let decoded = aria2_core::util::uri::percent_decode(encoded);
        decode_aria2_base64(&decoded)
    });

    let body = match (method, id) {
        (None, None) => decoded_params.unwrap_or_default(),
        (method, id) => {
            let mut body = Vec::new();
            body.extend_from_slice(b"{");
            if let Some(method) = method {
                body.extend_from_slice(b"\"method\":\"");
                body.extend_from_slice(method.as_bytes());
                body.extend_from_slice(b"\"");
            }
            if let Some(id) = id {
                // The leading comma when `method` is absent is an observable
                // quirk of aria2_original's string builder.
                body.extend_from_slice(b",\"id\":\"");
                body.extend_from_slice(id.as_bytes());
                body.extend_from_slice(b"\"");
            }
            if has_params {
                let params = decoded_params
                    .as_deref()
                    .expect("non-empty params must have a decoded value");
                body.extend_from_slice(b",\"params\":");
                body.extend_from_slice(params);
            }
            body.extend_from_slice(b"}");
            body
        }
    };

    JsonGetRequest {
        body,
        // aria2_original emits this value verbatim as JavaScript. In
        // particular, it does not percent-decode or validate the callback.
        callback: callback.map(str::to_owned),
    }
}

/// Decode the permissive standard-Base64 stream used by aria2_original.
/// Invalid alphabet bytes are skipped and malformed input becomes an empty
/// or partial byte string; JSON parsing later produces the wire-level parse
/// error. This is deliberately separate from strict RPC parameter decoding.
fn decode_aria2_base64(input: &str) -> Vec<u8> {
    use base64::Engine;

    let filtered: Vec<u8> = input
        .bytes()
        .filter(|byte| {
            matches!(
                byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'+'
                    | b'/'
                    | b'='
            )
        })
        .collect();

    if filtered.is_empty() {
        return Vec::new();
    }

    let input = if let Some(eq_pos) = filtered.iter().position(|byte| *byte == b'=') {
        let group_start = eq_pos / 4 * 4;
        let group_end = group_start + 4;
        if group_end > filtered.len()
            || filtered[eq_pos..group_end].iter().any(|byte| *byte != b'=')
        {
            return Vec::new();
        }
        &filtered[..group_end]
    } else if filtered.len() % 4 == 1 {
        // A lone trailing alphabet byte is ignored by aria2_original after
        // all complete quartets have been decoded.
        &filtered[..filtered.len() - 1]
    } else {
        &filtered
    };

    base64::engine::general_purpose::STANDARD
        .decode(input)
        .unwrap_or_default()
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
    use crate::json_rpc::{JsonRpcBatchResponse, JsonRpcWireEntry, parse_aria2_wire_document};

    match parse_aria2_wire_document(body) {
        Ok(document) if !document.is_batch => {
            let entry = document
                .entries
                .into_iter()
                .next()
                .expect("single JSON-RPC document must contain one entry");
            let response = match entry {
                JsonRpcWireEntry::Request(request) => engine.handle_request(&request).await,
                JsonRpcWireEntry::Error(response) => response,
            };
            serialize_jsonrpc_response(response)
        }
        Ok(document) => {
            let mut responses = Vec::with_capacity(document.entries.len());
            for entry in document.entries {
                responses.push(match entry {
                    JsonRpcWireEntry::Request(request) => engine.handle_request(&request).await,
                    JsonRpcWireEntry::Error(response) => response,
                });
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

    let (status, content_type, response_body) = match parse_request(&body) {
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
                                // Once XML-RPC parsing has succeeded, aria2
                                // reports method-side failures as faultCode=1
                                // regardless of the JSON-RPC adapter code.
                                XmlRpcResponse::fault(1, &error.to_string())
                            }),
                        None => {
                            let message = json_response
                                .error
                                .map(|error| error.message)
                                .unwrap_or_else(|| "Missing RPC response".into());
                            XmlRpcResponse::fault(1, &message)
                        }
                    };
                    (StatusCode::OK, Some("text/xml"), response.to_xml())
                }
                // aria2_original treats XML value conversion failures as
                // request parse failures: HTTP 400 with an empty body.
                Err(_) => (StatusCode::BAD_REQUEST, None, String::new()),
            }
        }
        // Keep the original HTTP/XML-RPC split. The C++ body command sends
        // `feedResponse(400)` for parser errors, which has no XML fault body.
        Err(_) => (StatusCode::BAD_REQUEST, None, String::new()),
    };

    let mut response = axum::response::Response::new(axum::body::Body::from(response_body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(content_type),
        );
    }
    response
}

/// Handle GET requests at `/jsonrpc`.
///
/// Supports WebSocket upgrades and aria2's legacy GET/JSONP transport.
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
            let parsed = parse_json_get_query(query.0.as_deref().unwrap_or_default());
            let response = dispatch_jsonrpc_body(&state.engine, &parsed.body).await;
            let content_type = if parsed.callback.is_some() {
                "text/javascript"
            } else {
                "application/json-rpc"
            };
            (
                response.status,
                [(axum::http::header::CONTENT_TYPE, content_type)],
                wrap_jsonp(response.body, parsed.callback.as_deref()),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_get_query_matches_aria2_wire_grammar() {
        use base64::Engine;

        let encoded = base64::engine::general_purpose::STANDARD.encode("[]");
        let parsed = parse_json_get_query(&format!(
            "method=aria2.getVersion&id=foo%20bar&params={encoded}&jsoncallback=cb%2Ename"
        ));

        assert_eq!(
            parsed.body,
            br#"{"method":"aria2.getVersion","id":"foo%20bar","params":[]}"#
        );
        assert_eq!(parsed.callback.as_deref(), Some("cb%2Ename"));
    }

    #[test]
    fn test_json_get_query_keeps_original_malformed_cases_for_json_parser() {
        let no_query = parse_json_get_query("");
        assert!(no_query.body.is_empty());

        let id_only = parse_json_get_query("id=only-id");
        assert_eq!(id_only.body, br#"{,"id":"only-id"}"#);

        let invalid_base64 = parse_json_get_query("method=aria2.getVersion&params=not-base64");
        assert!(!invalid_base64.body.is_empty());
    }

    #[test]
    fn test_json_get_query_omits_empty_params_like_aria2() {
        let parsed = parse_json_get_query("method=aria2.getVersion&id=empty&params=");
        assert_eq!(
            parsed.body,
            br#"{"method":"aria2.getVersion","id":"empty"}"#
        );
    }

    #[test]
    fn test_json_get_callback_is_not_normalized() {
        let parsed = parse_json_get_query("jsoncallback=bad;alert(1)//");
        assert_eq!(parsed.callback.as_deref(), Some("bad;alert(1)//"));
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
