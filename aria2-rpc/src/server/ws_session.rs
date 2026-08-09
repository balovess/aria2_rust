//! WebSocket session logic for the RPC server: upgrade handling, inbound
//! JSON-RPC dispatch, and outbound event forwarding.

use std::sync::Arc;

use super::http_routes::RpcState;
use crate::engine::RpcEngine;

/// Handle WebSocket upgrade requests.
///
/// Upgrades HTTP connections to WebSocket protocol for real-time
/// download event notifications (progress, completion, errors, etc.).
pub async fn ws_handler(
    axum::extract::State(state): axum::extract::State<RpcState>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    // Enforce max frame/message size to prevent OOM from oversized payloads.
    let max_size = state.max_request_size;
    ws.max_frame_size(max_size)
        .max_message_size(max_size)
        .on_upgrade(move |socket| handle_ws_socket(socket, state.engine.clone()))
}

/// Handle an upgraded WebSocket connection.
///
/// Serves dual purpose (matching C++ aria2's `WebSocketSession::onMsgRecvCallback`):
/// - **OUTBOUND**: Subscribes to the engine's event publisher and forwards
///   download event notifications to the connected WebSocket client.
/// - **INBOUND**: Processes incoming `Text` messages as JSON-RPC requests
///   (single or batch), dispatches them through `RpcEngine::handle_request`,
///   and sends the response(s) back over the WebSocket.
///
/// The `tokio::select!` loop interleaves both directions so that event
/// notifications continue flowing even while request/response traffic is
/// active on the same connection.
pub async fn handle_ws_socket(
    mut socket: axum::extract::ws::WebSocket,
    engine: std::sync::Arc<crate::engine::RpcEngine>,
) {
    use tokio::sync::broadcast;

    // Subscribe to the engine's event publisher
    let mut rx = engine.event_publisher.subscribe("ws-conn", None).await;

    loop {
        tokio::select! {
            // Wait for events from the engine (outbound notifications)
            result = rx.recv() => {
                match result {
                    Ok((_event_type, event)) => {
                        if let Ok(json_str) = event.to_json()
                            && socket
                                .send(axum::extract::ws::Message::Text(json_str))
                                .await
                                .is_err()
                            {
                                break; // Client disconnected
                            }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket client lagged by {} events", n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break; // Engine shut down
                    }
                }
            }

            // Wait for incoming messages from client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(axum::extract::ws::Message::Text(text))) => {
                        // Process incoming text as a JSON-RPC request
                        process_ws_jsonrpc(&mut socket, &engine, &text).await;
                    }
                    Some(Ok(axum::extract::ws::Message::Close(frame))) => {
                        tracing::debug!(?frame, "WebSocket close frame received");
                        let _ = socket
                            .send(axum::extract::ws::Message::Close(frame))
                            .await;
                        break;
                    }
                    None => {
                        tracing::debug!("WebSocket stream ended without a close frame");
                        break;
                    }
                    Some(Ok(axum::extract::ws::Message::Ping(data))) => {
                        // Respond to ping with pong
                        let _ = socket
                            .send(axum::extract::ws::Message::Pong(data))
                            .await;
                    }
                    Some(Err(e)) => {
                        tracing::warn!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {
                        // Ignore other message types (pong, binary, etc.)
                        continue;
                    }
                }
            }
        }
    }

    tracing::debug!("WebSocket client disconnected");
}

/// Process an incoming WebSocket text message as a JSON-RPC request.
///
/// Mirrors C++ aria2's `WebSocketSession::onMsgRecvCallback`:
/// 1. Parse the text as JSON (single object or batch array).
/// 2. If parse fails → send a JSON-RPC Parse Error (-32700) response.
/// 3. If valid single request → dispatch through `RpcEngine::handle_request`,
///    send one response object.
/// 4. If valid batch request → dispatch each element, send a JSON array of
///    responses.
/// 5. If the JSON value is neither object nor array → send Invalid Request
///    (-32600) response.
async fn process_ws_jsonrpc(
    socket: &mut axum::extract::ws::WebSocket,
    engine: &Arc<RpcEngine>,
    text: &str,
) {
    use crate::json_rpc::{JsonRpcBatchResponse, JsonRpcWireEntry, parse_aria2_wire_document};

    // Step 1: Parse the incoming text as JSON-RPC request(s)
    let document = match parse_aria2_wire_document(text.as_bytes()) {
        Ok(document) => document,
        Err(e) => {
            // Step 2: Parse error → send -32700 response
            tracing::warn!("WebSocket JSON-RPC parse error: {}", e);
            let resp = e.into_response(None);
            send_ws_response(socket, &resp).await;
            return;
        }
    };

    // Step 3/4: Dispatch request(s) through the engine
    if !document.is_batch {
        // Single request — send single response object (not wrapped in array)
        let entry = document
            .entries
            .into_iter()
            .next()
            .expect("single JSON-RPC document must contain one entry");
        let resp = match entry {
            JsonRpcWireEntry::Request(request) => engine.handle_request(&request).await,
            JsonRpcWireEntry::Error(response) => response,
        };
        send_ws_response(socket, &resp).await;
    } else {
        // Batch request — send array of response objects
        let mut results = Vec::with_capacity(document.entries.len());
        for entry in document.entries {
            results.push(match entry {
                JsonRpcWireEntry::Request(request) => engine.handle_request(&request).await,
                JsonRpcWireEntry::Error(response) => response,
            });
        }
        let batch = JsonRpcBatchResponse(results);
        match batch.to_string() {
            Ok(json_str) => {
                if socket
                    .send(axum::extract::ws::Message::Text(json_str))
                    .await
                    .is_err()
                {
                    tracing::debug!("Failed to send WS batch response (client disconnected)");
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize WS batch response: {}", e);
            }
        }
    }
}

/// Send a single JSON-RPC response over the WebSocket connection.
async fn send_ws_response(
    socket: &mut axum::extract::ws::WebSocket,
    resp: &crate::json_rpc::JsonRpcResponse,
) {
    match resp.to_string() {
        Ok(json_str) => {
            if socket
                .send(axum::extract::ws::Message::Text(json_str))
                .await
                .is_err()
            {
                tracing::debug!("Failed to send WS response (client disconnected)");
            }
        }
        Err(e) => {
            tracing::error!("Failed to serialize WS JSON-RPC response: {}", e);
        }
    }
}
