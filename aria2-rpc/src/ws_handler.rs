//! WebSocket upgrade handler for the `/jsonrpc` endpoint.
//!
//! Mirrors the original aria2 WebSocket semantics on the same `/jsonrpc`
//! route (see `WebSocketSession.cc::onMsgRecvCallback` and
//! `WebSocketSessionMan::addNotification` in the upstream C++ source):
//!
//! * The same path that serves POST JSON-RPC also accepts an HTTP
//!   `Upgrade: websocket` request.
//! * Once upgraded, the client may send JSON-RPC requests (single or batch)
//!   as text frames and receive JSON-RPC responses.
//! * The server proactively broadcasts download notifications
//!   (`aria2.onDownloadStart`, `aria2.onDownloadComplete`, ...) to every
//!   connected WebSocket client — there is no explicit subscribe handshake.
//!
//! All notification frames use the canonical aria2 wire format
//! `{"jsonrpc":"2.0","method":"aria2.onDownloadStart","params":[{"gid":"<hex>"}]}`
//! produced by `DownloadEvent`.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::engine::RpcEngine;
use crate::json_rpc::{JsonRpcError, JsonRpcResponse, parse_request};
use crate::server::RpcState;

/// Maximum number of outbound frames buffered per WebSocket connection.
///
/// Protects against a slow client stalling notification dispatch: if the
/// per-connection channel fills up, additional notifications are dropped
/// (with a tracing warning) rather than blocking the publisher task.
const WS_OUTBOUND_CHANNEL_CAPACITY: usize = 256;

/// Handle an upgraded WebSocket connection.
///
/// Three concurrent tasks cooperate:
/// 1. **Reader** — reads text frames from the client, parses them as
///    JSON-RPC requests (single or batch), dispatches them through the
///    shared `RpcEngine`, and forwards the responses to the writer channel.
/// 2. **Writer** — drains the writer channel and sends each frame to the
///    client, exiting when all senders are dropped.
/// 3. **Notifier** — subscribes to the engine's `EventPublisher` and
///    forwards each broadcast event to the writer channel.
///
/// The reader is the task owning the connection: when it returns
/// (client disconnect, close frame, or fatal error), the writer and
/// notifier tasks are cancelled via the channel-drop + abort pattern.
pub(crate) async fn handle_ws_connection(socket: WebSocket, state: RpcState) {
    // Use an explicit subscriber ID so the EventPublisher tracking HashMap
    // does not accumulate stale entries when the connection drops.
    let subscriber_id = format!("ws-{}", unique_id());
    debug!(%subscriber_id, "WebSocket session connected");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Outbound frame channel — multiplexes JSON-RPC responses and notifications.
    let (tx, mut rx) = mpsc::channel::<String>(WS_OUTBOUND_CHANNEL_CAPACITY);

    // ---- Writer task: drain the channel and send frames -------------------
    // The writer owns `ws_sender`. It exits when all senders (reader `tx` +
    // notifier `tx_events`) are dropped, which happens during cleanup below.
    let writer_handle = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(err) = ws_sender.send(Message::Text(frame)).await {
                warn!("WebSocket send failed: {err}; closing connection");
                break;
            }
        }
    });

    // ---- Notifier task: subscribe to EventPublisher broadcasts -------------
    let engine: Arc<RpcEngine> = state.engine.clone();
    let mut event_rx = engine
        .publisher()
        .subscribe(subscriber_id.clone(), None)
        .await;
    let notifier_handle = {
        let tx_events = tx.clone();
        let sub_id = subscriber_id.clone();
        tokio::spawn(async move {
            while let Ok((_event_type, event)) = event_rx.recv().await {
                match event.to_json() {
                    Ok(json) => {
                        if tx_events.send(json).await.is_err() {
                            debug!(%sub_id, "WebSocket writer exited; stopping notifier");
                            break;
                        }
                    }
                    Err(err) => {
                        error!(%sub_id, "Failed to serialize download event: {err}");
                    }
                }
            }
        })
    };

    // ---- Reader task: own the connection lifetime -------------------------
    // Run inline (no extra spawn) so that this function returning marks the
    // end of the connection and triggers cleanup below.
    let reader_engine = state.engine.clone();
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                handle_inbound_text(&reader_engine, text, &tx).await;
            }
            Ok(Message::Binary(_bytes)) => {
                // Original aria2 only processes text frames; reject binary.
                let err = JsonRpcError::InvalidRequest(
                    "binary frames are not supported; send text only".to_string(),
                );
                let resp = err.into_response(None);
                let payload = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
                let _ = tx.send(payload).await;
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                // axum handles ping/pong automatically; ignore the userland copy.
            }
            Ok(Message::Close(_)) => {
                debug!(%subscriber_id, "WebSocket close frame received");
                break;
            }
            Err(err) => {
                warn!(%subscriber_id, "WebSocket receive error: {err}");
                break;
            }
        }
    }

    // ---- Cleanup: drop senders, abort notifier, unsubscribe ---------------
    drop(tx);
    // Closing the writer channel allows the writer to flush any remaining
    // buffered frames before exiting; abort immediately is unsafe because we
    // might lose pending notifications. We await the writer instead.
    let _ = writer_handle.await;
    notifier_handle.abort();
    engine.publisher().unsubscribe(&subscriber_id).await;
    debug!(%subscriber_id, "WebSocket session closed");
}

/// Dispatch a single inbound text frame: parse, run, and enqueue the response.
///
/// Supports both single JSON-RPC requests and batch requests
/// (a JSON array of requests). Parse errors are reported using the
/// canonical JSON-RPC error envelope so the client can correlate the failure.
async fn handle_inbound_text(engine: &Arc<RpcEngine>, text: String, tx: &mpsc::Sender<String>) {
    let response_payload: String = match parse_request(text.as_bytes()) {
        Ok(requests) if requests.len() == 1 => {
            let req = &requests[0];
            let response = engine.handle_request(req).await;
            serde_json::to_string(&response).unwrap_or_else(|_| fallback_error())
        }
        Ok(requests) => {
            // Batch: dispatch each request concurrently and respond with an array.
            let futures_iter = requests
                .iter()
                .map(|req| engine.handle_request(req))
                .collect::<Vec<_>>();
            let responses: Vec<JsonRpcResponse> = futures::future::join_all(futures_iter).await;
            serde_json::to_string(&responses).unwrap_or_else(|_| fallback_error())
        }
        Err(err) => {
            let response = err.into_response(None);
            serde_json::to_string(&response).unwrap_or_else(|_| fallback_error())
        }
    };

    if let Err(_) = tx.send(response_payload).await {
        debug!("Writer channel closed; dropping inbound response");
    }
}

/// Fallback envelope used only if serde itself fails to serialize a response.
///
/// Should never trigger in practice but guarantees the client always gets a
/// well-formed JSON object back even under catastrophic conditions.
fn fallback_error() -> String {
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal serialization failure"}}"#
        .to_string()
}

/// Generate a short, statistically-unique subscriber/connection identifier.
///
/// Uses a process-local counter combined with a coarse timestamp — sufficient
/// for subscriber disambiguation; collision is irrelevant because the
/// EventPublisher only uses the id as a HashMap key for `unsubscribe()`.
fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}", n)
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::RpcEngine;
    use crate::json_rpc::JsonRpcRequest;
    use crate::server::RpcState;
    use crate::websocket::DownloadEvent;
    use serde_json::json;

    #[tokio::test]
    async fn handle_inbound_text_single_request_returns_success() {
        let engine = Arc::new(RpcEngine::new());
        let (tx, mut rx) = mpsc::channel::<String>(8);

        let req = JsonRpcRequest::new("aria2.getVersion", json!([])).with_id(1);
        let req_text = serde_json::to_string(&req).unwrap();

        handle_inbound_text(&engine, req_text, &tx).await;

        let resp = rx.recv().await.expect("response should be sent on channel");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert!(v["result"]["version"].is_string());
    }

    #[tokio::test]
    async fn handle_inbound_text_batch_request_returns_array() {
        let engine = Arc::new(RpcEngine::new());
        let (tx, mut rx) = mpsc::channel::<String>(8);

        let batch = json!([
            {"jsonrpc": "2.0", "method": "aria2.getVersion", "params": [], "id": 1},
            {"jsonrpc": "2.0", "method": "aria2.getGlobalStat", "params": [], "id": 2}
        ]);
        let batch_text = serde_json::to_string(&batch).unwrap();

        handle_inbound_text(&engine, batch_text, &tx).await;

        let resp = rx.recv().await.expect("batch response should be sent");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[1]["id"], 2);
    }

    #[tokio::test]
    async fn handle_inbound_text_invalid_json_returns_parse_error_envelope() {
        let engine = Arc::new(RpcEngine::new());
        let (tx, mut rx) = mpsc::channel::<String>(8);

        handle_inbound_text(&engine, "not valid json".to_string(), &tx).await;

        let resp = rx.recv().await.expect("error response should be sent");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["error"]["code"], -32700,
            "expected JSON-RPC parse error code"
        );
    }

    #[tokio::test]
    async fn handle_inbound_text_authenticated_request_strips_token() {
        // Build an engine with auth, verify the positional `token:secret`
        // param is stripped before dispatch (aria2 protocol).
        let engine = Arc::new(
            RpcEngine::new().with_auth_middleware(crate::server::RpcAuthMiddleware::new("shhh")),
        );
        let (tx, mut rx) = mpsc::channel::<String>(8);

        let req = json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": ["token:shhh"],
            "id": 7
        });
        handle_inbound_text(&engine, serde_json::to_string(&req).unwrap(), &tx).await;

        let resp = rx.recv().await.expect("response should be sent");
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert!(v.get("error").is_none(), "auth should pass for valid token");
        assert_eq!(v["result"]["version"].as_str().is_some(), true);
    }

    #[tokio::test]
    async fn handle_ws_connection_forwards_notifications_to_clients() {
        // End-to-end: open a WebSocket, publish a DownloadEvent, assert the
        // notification arrives on the wire.
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        // Stand up a tiny axum router that only mounts the WS route.
        let engine = Arc::new(RpcEngine::new());
        let state = RpcState {
            engine: engine.clone(),
        };
        let app = axum::Router::new()
            .route(
                "/jsonrpc",
                axum::routing::get(
                    |axum::extract::State(state): axum::extract::State<RpcState>,
                     ws_upgrade: Option<axum::extract::ws::WebSocketUpgrade>| async move {
                        use axum::response::IntoResponse;
                        match ws_upgrade {
                            Some(ws) => ws
                                .on_upgrade(move |socket| handle_ws_connection(socket, state))
                                .into_response(),
                            None => (
                                axum::http::StatusCode::METHOD_NOT_ALLOWED,
                                "WebSocket upgrade required",
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Connect a client via tungstenite.
        let url = format!("ws://{addr}/jsonrpc");
        let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");

        // Publish an event from the engine and verify the client receives it.
        let gid = "0000000000000001";
        let event = DownloadEvent::download_start(gid);
        engine
            .publisher()
            .publish_event(event)
            .expect("publish should succeed");

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next())
            .await
            .expect("should receive notification within timeout")
            .expect("stream should not end")
            .expect("ws stream should not produce an error");

        match msg {
            TungsteniteMessage::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["jsonrpc"], "2.0");
                assert_eq!(v["method"], "aria2.onDownloadStart");
                assert_eq!(v["params"][0]["gid"], gid);
                // Verify no extra fields pollute the params object.
                let params_obj = v["params"][0].as_object().unwrap();
                assert_eq!(
                    params_obj.len(),
                    1,
                    "params[0] should contain only gid, got: {params_obj:?}"
                );
            }
            other => panic!("expected text message, got {other:?}"),
        }

        // Cleanly close the client (no specific close code needed for the test).
        ws_stream.close(None).await.ok();
        server_handle.abort();
    }

    #[tokio::test]
    async fn handle_ws_connection_round_trips_jsonrpc_request() {
        // Open a WS, send a JSON-RPC request frame, assert a well-formed
        // response frame comes back over the same socket.
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        let engine = Arc::new(RpcEngine::new());
        let state = RpcState {
            engine: engine.clone(),
        };
        let app = axum::Router::new()
            .route(
                "/jsonrpc",
                axum::routing::get(
                    |axum::extract::State(state): axum::extract::State<RpcState>,
                     ws_upgrade: Option<axum::extract::ws::WebSocketUpgrade>| async move {
                        use axum::response::IntoResponse;
                        match ws_upgrade {
                            Some(ws) => ws
                                .on_upgrade(move |socket| handle_ws_connection(socket, state))
                                .into_response(),
                            None => (
                                axum::http::StatusCode::METHOD_NOT_ALLOWED,
                                "WebSocket upgrade required",
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("ws://{addr}/jsonrpc");
        let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");

        // Send a JSON-RPC request as a text frame.
        let req = json!({
            "jsonrpc": "2.0",
            "method": "aria2.getVersion",
            "params": [],
            "id": 42
        });
        ws_stream
            .send(TungsteniteMessage::Text(
                serde_json::to_string(&req).unwrap(),
            ))
            .await
            .expect("send");

        // Read until we see a non-notification message with id=42.
        // (Notifications may interleave, so we filter.)
        let mut got_response = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), ws_stream.next())
                .await
            {
                Ok(Some(Ok(TungsteniteMessage::Text(text)))) => {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if v.get("id") == Some(&serde_json::json!(42)) {
                        assert!(v.get("result").is_some(), "expected result field, got: {v}");
                        assert!(v["result"]["version"].is_string());
                        got_response = true;
                        break;
                    }
                    // Otherwise it's a notification, ignore for this test.
                }
                _ => continue,
            }
        }
        assert!(
            got_response,
            "did not receive response for id=42 within timeout"
        );

        ws_stream.close(None).await.ok();
        server_handle.abort();
    }
}
