//! HTTP request handlers and transport-specific response shaping.

use std::sync::Arc;

use super::auth::AuthConfig;
use super::cors::CorsConfig;
use super::ws_session::handle_ws_socket;
use crate::engine::{RpcEngine, dispatch_wire_entries};

/// Shared state for RPC handlers
#[derive(Clone)]
pub(super) struct RpcState {
    pub(crate) engine: Arc<RpcEngine>,
    /// HTTP Basic Auth configuration. Token auth remains in `RpcEngine` and
    /// is carried in the JSON/XML-RPC parameter contract.
    pub(crate) auth: AuthConfig,
    /// Maximum JSON/XML-RPC parser input size in bytes.
    pub(crate) max_request_size: usize,
}

/// Marks a response that must become a connection close without wire bytes.
///
/// The marker is consumed by the low-level HTTP/TLS connection services after
/// the regular Router middleware has preserved original authentication order.
#[derive(Clone, Debug)]
pub(super) struct DropHttpConnection;

/// Convert the public CORS configuration into tower-http's request-aware
/// layer. `AllowOrigin::list` mirrors an allowed origin back to the browser,
/// while wildcard mode retains aria2's literal `*` response.
pub(super) fn build_cors_layer(config: &CorsConfig) -> tower_http::cors::CorsLayer {
    use axum::http::{HeaderName, HeaderValue, Method};
    use std::time::Duration;
    use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

    // An empty origin list is the aria2_original default: do not emit any
    // CORS response or preflight headers until the user opts in.
    if config.allowed_origins().is_empty() {
        return CorsLayer::new();
    }

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
    let max_age = crate::constants::CORS_MAX_AGE;

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
pub(super) async fn http_auth_middleware(
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
        if request_content_length_exceeds_limit(&request, state.max_request_size) {
            let mut response = axum::response::Response::new(axum::body::Body::empty());
            response.extensions_mut().insert(DropHttpConnection);
            return response;
        }
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

/// Match `HttpServerCommand`: only non-WebSocket requests with a declared
/// Content-Length greater than the configured cap are dropped at header time.
fn request_content_length_exceeds_limit(
    request: &axum::extract::Request,
    max_request_size: usize,
) -> bool {
    use axum::http::header;

    let is_websocket_upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && request
            .headers()
            .get(header::CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
    if is_websocket_upgrade {
        return false;
    }

    request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|content_length| content_length > max_request_size as u64)
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
        let decoded = crate::rpc_helpers::percent_decode(encoded);
        crate::rpc_helpers::decode_aria2_base64(&decoded)
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

struct JsonRpcHttpResponse {
    status: axum::http::StatusCode,
    body: Vec<u8>,
    close_connection: bool,
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
    let body = response.to_bytes().unwrap_or_else(|error| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32603,\"message\":{}}}}}",
            serde_json::Value::String(error.to_string())
        )
        .into_bytes()
    });
    JsonRpcHttpResponse {
        status,
        body,
        // `HttpServerBodyCommand::sendJsonRpcResponse` disables keep-alive
        // for every single-response error. Batch responses use a separate
        // original path and remain eligible for connection reuse.
        close_connection: response.error.is_some(),
    }
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
                JsonRpcWireEntry::Request(request) => engine.handle_request_owned(request).await,
                JsonRpcWireEntry::Error(response) => response,
            };
            serialize_jsonrpc_response(response)
        }
        Ok(document) => {
            let body = JsonRpcBatchResponse(dispatch_wire_entries(engine, document.entries).await)
                .to_bytes()
                .unwrap_or_else(|error| {
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32603,\"message\":{}}}}}",
                        serde_json::Value::String(error.to_string())
                    )
                    .into_bytes()
                });
            // aria2 always returns HTTP 200 for a batch envelope, even when
            // individual entries contain RPC errors.
            JsonRpcHttpResponse {
                status: axum::http::StatusCode::OK,
                body,
                close_connection: false,
            }
        }
        Err(error) => serialize_jsonrpc_response(error.into_response(None)),
    }
}

fn into_jsonrpc_http_response(
    response: JsonRpcHttpResponse,
    content_type: &'static str,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header};
    use axum::response::IntoResponse;

    let close_connection = response.close_connection;
    let mut http_response = (
        response.status,
        [(header::CONTENT_TYPE, content_type)],
        response.body,
    )
        .into_response();
    if close_connection {
        http_response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    http_response
}

fn wrap_jsonp(body: Vec<u8>, callback: Option<&str>) -> Vec<u8> {
    match callback {
        Some(callback) => {
            let mut wrapped = Vec::with_capacity(callback.len() + body.len() + 2);
            wrapped.extend_from_slice(callback.as_bytes());
            wrapped.push(b'(');
            wrapped.extend_from_slice(&body);
            wrapped.push(b')');
            wrapped
        }
        None => body,
    }
}

pub(super) async fn handle_jsonrpc(
    axum::extract::State(state): axum::extract::State<RpcState>,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    let response = dispatch_jsonrpc_body(&state.engine, &body).await;
    into_jsonrpc_http_response(response, "application/json-rpc")
}

/// Handle the original aria2 XML-RPC endpoint at `/rpc`.
pub(super) async fn handle_xmlrpc(
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
/// 2. **Regular GET** — Dispatches aria2's legacy GET/JSONP transport.
///
/// Upstream aria2 accepts WebSocket upgrades only at `/jsonrpc`; regular GET
/// requests at that same path retain its legacy JSONP behavior.
pub(super) async fn handle_jsonrpc_or_ws(
    axum::extract::State(state): axum::extract::State<RpcState>,
    ws: Option<axum::extract::ws::WebSocketUpgrade>,
    query: axum::extract::RawQuery,
) -> impl axum::response::IntoResponse {
    match ws {
        Some(upgrade) => {
            // WebSocket upgrade request from Aria2 Explorer or other clients.
            // An oversized RPC document receives a JSON-RPC parse error rather
            // than a transport-level disconnect.
            let max_request_size = state.max_request_size;
            upgrade.on_upgrade(move |socket| {
                handle_ws_socket(socket, state.engine.clone(), max_request_size)
            })
        }
        None => {
            let parsed = parse_json_get_query(query.0.as_deref().unwrap_or_default());
            let response = dispatch_jsonrpc_body(&state.engine, &parsed.body).await;
            let content_type = if parsed.callback.is_some() {
                "text/javascript"
            } else {
                "application/json-rpc"
            };
            let body = wrap_jsonp(response.body, parsed.callback.as_deref());
            into_jsonrpc_http_response(JsonRpcHttpResponse { body, ..response }, content_type)
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
}
