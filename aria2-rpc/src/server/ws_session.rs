//! WebSocket session logic for the RPC server: upgrade handling, inbound
//! JSON-RPC dispatch, and outbound event forwarding.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::RpcEngine;

static NEXT_WS_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

/// Handle an upgraded WebSocket connection.
///
/// Serves dual purpose (matching C++ aria2's `WebSocketSession::onMsgRecvCallback`):
/// - **OUTBOUND**: Subscribes to the engine's event publisher and forwards
///   download event notifications to the connected WebSocket client.
/// - **INBOUND**: Processes incoming non-control WebSocket message payloads
///   as JSON-RPC requests (single or batch), dispatches them through
///   `RpcEngine::handle_request`, and sends the response(s) back over the
///   WebSocket. This accepts both text and binary frames like aria2_original.
///
/// The `tokio::select!` loop interleaves both directions so that event
/// notifications continue flowing even while request/response traffic is
/// active on the same connection.
pub async fn handle_ws_socket(
    mut socket: axum::extract::ws::WebSocket,
    engine: std::sync::Arc<crate::engine::RpcEngine>,
    max_request_size: usize,
) {
    use tokio::sync::broadcast;

    // C++ owns one WebSocketSession entry per connection and removes it on
    // disconnect. Keep the same lifecycle with a unique RAII subscription.
    let subscriber_id = format!(
        "ws-conn-{}",
        NEXT_WS_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed)
    );
    let mut subscription = engine.event_publisher.subscribe_scoped(subscriber_id, None);

    loop {
        tokio::select! {
            // Wait for events from the engine (outbound notifications)
            result = subscription.recv() => {
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
                        process_ws_jsonrpc(&mut socket, &engine, text.as_bytes(), max_request_size).await;
                    }
                    Some(Ok(axum::extract::ws::Message::Binary(data))) => {
                        // aria2_original sends every non-control frame through
                        // its JSON parser, including binary frames.
                        process_ws_jsonrpc(&mut socket, &engine, data.as_ref(), max_request_size).await;
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
                        // Ignore Pong and any future message variants.
                        continue;
                    }
                }
            }
        }
    }

    tracing::debug!("WebSocket client disconnected");
}

/// Process an incoming WebSocket message payload as a JSON-RPC request.
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
    body: &[u8],
    max_request_size: usize,
) {
    use crate::json_rpc::{JsonRpcBatchResponse, JsonRpcWireEntry, parse_aria2_wire_document};

    // aria2_original stops feeding bytes into its incremental JSON parser
    // once the configured cap is exceeded. Finalization then produces the
    // normal JSON-RPC parse error without closing the WebSocket session.
    let body = if body.len() > max_request_size {
        tracing::info!(
            request_size = body.len(),
            max_request_size,
            "WebSocket JSON-RPC request exceeds parser limit"
        );
        &[]
    } else {
        body
    };

    // Step 1: Parse the incoming text as JSON-RPC request(s)
    let document = match parse_aria2_wire_document(body) {
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
