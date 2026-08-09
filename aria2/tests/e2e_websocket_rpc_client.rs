//! Process-level WebSocket JSON-RPC and notification compatibility coverage.

mod support;

use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use support::RunningAria2;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(socket: &mut ClientSocket) -> Value {
    loop {
        let message = tokio::time::timeout(MESSAGE_TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for WebSocket message")
            .expect("WebSocket stream ended before expected message")
            .expect("WebSocket client received an error");
        match message {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .expect("aria2 WebSocket text message must be JSON");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(&bytes)
                    .expect("aria2 WebSocket binary message must be JSON");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(frame) => panic!("WebSocket closed before expected message: {frame:?}"),
            _ => continue,
        }
    }
}

async fn wait_for_response(socket: &mut ClientSocket, id: &str) -> Value {
    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for JSON-RPC response id={id}"
        );
        let message = next_json(socket).await;
        if message.get("id").and_then(Value::as_str) == Some(id) {
            return message;
        }
    }
}

/// A browser-style WebSocket client must be able to make JSON-RPC calls and
/// receive aria2's start/stop notifications over the same `/jsonrpc`
/// connection.  The C++ server always replies with text JSON, so this checks
/// the complete externally observable sequence rather than an in-process
/// publisher abstraction.
#[tokio::test]
async fn e2e_websocket_jsonrpc_client_receives_download_start_and_stop_notifications() {
    let secret = "websocket-process-secret";
    let mut aria2 = RunningAria2::start_rpc(&[format!("--rpc-secret={secret}")]);
    let ws_url = format!("ws://127.0.0.1:{}/jsonrpc", aria2.port());
    let (mut socket, _) = connect_async(ws_url)
        .await
        .expect("WebSocket upgrade at /jsonrpc must succeed");

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.getVersion",
                "params": [format!("token:{secret}")],
                "id": "version",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must send getVersion");
    let version = wait_for_response(&mut socket, "version").await;
    assert_eq!(version["jsonrpc"], "2.0");
    assert!(
        version["result"]["version"].is_string(),
        "getVersion must preserve aria2's string version field: {version}"
    );

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.addUri",
                "params": [
                    format!("token:{secret}"),
                    ["http://127.0.0.1:1/websocket-e2e.bin"],
                    {"out": "websocket-e2e.bin"},
                ],
                "id": "add",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must send addUri");

    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let mut added_gid = None;
    let mut started_gids = Vec::new();
    while added_gid.is_none() || started_gids.is_empty() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for addUri response and start notification"
        );
        let message = next_json(&mut socket).await;
        if message.get("id").and_then(Value::as_str) == Some("add") {
            added_gid = message["result"].as_str().map(str::to_owned);
            assert!(
                added_gid.is_some(),
                "addUri must return aria2's string GID: {message}"
            );
            continue;
        }

        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(gid) = message
            .get("params")
            .and_then(Value::as_array)
            .and_then(|params| params.first())
            .and_then(|entry| entry.get("gid"))
            .and_then(Value::as_str)
        else {
            panic!("aria2 lifecycle notification must contain params[0].gid: {message}");
        };
        match method {
            "aria2.onDownloadStart" => started_gids.push(gid.to_owned()),
            _ => continue,
        }
    }

    let gid = added_gid.expect("addUri response must have been observed");
    assert!(
        started_gids.iter().all(|event_gid| event_gid == &gid),
        "onDownloadStart must carry the addUri GID: {started_gids:?}, expected {gid}"
    );

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.remove",
                "params": [format!("token:{secret}"), gid],
                "id": "remove",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must send remove");

    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let mut removed_gid = None;
    let mut stopped_gids = Vec::new();
    while removed_gid.is_none() || stopped_gids.is_empty() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for remove response and stop notification"
        );
        let message = next_json(&mut socket).await;
        if message.get("id").and_then(Value::as_str) == Some("remove") {
            removed_gid = message["result"].as_str().map(str::to_owned);
            assert_eq!(removed_gid.as_deref(), Some(gid.as_str()));
            continue;
        }

        if message.get("method").and_then(Value::as_str) == Some("aria2.onDownloadStop") {
            let event_gid = message
                .get("params")
                .and_then(Value::as_array)
                .and_then(|params| params.first())
                .and_then(|entry| entry.get("gid"))
                .and_then(Value::as_str)
                .expect("onDownloadStop must contain params[0].gid");
            stopped_gids.push(event_gid.to_owned());
        }
    }
    assert!(
        stopped_gids.iter().all(|event_gid| event_gid == &gid),
        "onDownloadStop must carry the addUri GID: {stopped_gids:?}, expected {gid}"
    );

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.shutdown",
                "params": [format!("token:{secret}")],
                "id": "shutdown",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must send shutdown");
    let shutdown = wait_for_response(&mut socket, "shutdown").await;
    assert!(
        shutdown["result"]
            .as_str()
            .is_some_and(|result| result.starts_with("OK.")),
        "shutdown must return aria2's successful result: {shutdown}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}

/// `aria2_original` limits WebSocket JSON parsing with
/// `rpc-max-request-size`, but keeps the socket alive and reports a JSON-RPC
/// parse error. A browser client can therefore recover on the same connection.
#[tokio::test]
async fn e2e_websocket_oversized_request_returns_parse_error_without_disconnect() {
    let mut aria2 = RunningAria2::start_rpc(&["--rpc-max-request-size=1K".to_owned()]);
    let ws_url = format!("ws://127.0.0.1:{}/jsonrpc", aria2.port());
    let (mut socket, _) = connect_async(ws_url)
        .await
        .expect("WebSocket upgrade at /jsonrpc must succeed");

    let oversized_request = json!({
        "jsonrpc": "2.0",
        "method": "aria2.getVersion",
        "params": [],
        "id": "too-large",
        "padding": "x".repeat(2 * 1024),
    })
    .to_string();
    assert!(
        oversized_request.len() > 1024,
        "fixture must exceed the configured rpc-max-request-size"
    );
    socket
        .send(Message::Text(oversized_request))
        .await
        .expect("WebSocket client must send oversized request");

    let parse_error = next_json(&mut socket).await;
    assert_eq!(parse_error["error"]["code"], -32700);
    assert_eq!(parse_error["id"], Value::Null);

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.getVersion",
                "params": [],
                "id": "after-parse-error",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must remain writable after parse error");
    let version = wait_for_response(&mut socket, "after-parse-error").await;
    assert!(
        version["result"]["version"].is_string(),
        "connection must remain usable after oversized request: {version}"
    );

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.forceShutdown",
                "params": [],
                "id": "shutdown",
            })
            .to_string(),
        ))
        .await
        .expect("WebSocket client must send shutdown");
    let shutdown = wait_for_response(&mut socket, "shutdown").await;
    assert!(
        shutdown["result"]
            .as_str()
            .is_some_and(|result| result.starts_with("OK.")),
        "forceShutdown must return aria2's successful result: {shutdown}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}

/// A browser client commonly reconnects after a transient WebSocket close.
/// The original session manager removes the old session, and the replacement
/// session must receive subsequent lifecycle notifications independently.
#[tokio::test]
async fn e2e_websocket_reconnect_receives_lifecycle_notifications() {
    let mut aria2 = RunningAria2::start_rpc(&[]);
    let ws_url = format!("ws://127.0.0.1:{}/jsonrpc", aria2.port());

    let (mut first_socket, _) = connect_async(&ws_url)
        .await
        .expect("first WebSocket upgrade must succeed");
    first_socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.getVersion",
                "params": [],
                "id": "first-connection",
            })
            .to_string(),
        ))
        .await
        .expect("first WebSocket client must send getVersion");
    let first_response = wait_for_response(&mut first_socket, "first-connection").await;
    assert!(first_response["result"]["version"].is_string());
    first_socket
        .close(None)
        .await
        .expect("first WebSocket client must close cleanly");
    drop(first_socket);

    let (mut socket, _) = connect_async(&ws_url)
        .await
        .expect("reconnected WebSocket upgrade must succeed");
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.addUri",
                "params": [["http://127.0.0.1:1/websocket-reconnect.bin"]],
                "id": "add-after-reconnect",
            })
            .to_string(),
        ))
        .await
        .expect("reconnected WebSocket client must send addUri");

    let deadline = Instant::now() + MESSAGE_TIMEOUT;
    let mut added_gid = None;
    let mut event_gid = None;
    while added_gid.is_none() || event_gid.is_none() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for addUri response and reconnect notification"
        );
        let message = next_json(&mut socket).await;
        if message.get("id").and_then(Value::as_str) == Some("add-after-reconnect") {
            added_gid = message["result"].as_str().map(str::to_owned);
            continue;
        }
        if message.get("method").and_then(Value::as_str) == Some("aria2.onDownloadStart") {
            event_gid = message
                .get("params")
                .and_then(Value::as_array)
                .and_then(|params| params.first())
                .and_then(|entry| entry.get("gid"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }
    assert_eq!(event_gid, added_gid);

    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "method": "aria2.forceShutdown",
                "params": [],
                "id": "shutdown",
            })
            .to_string(),
        ))
        .await
        .expect("reconnected WebSocket client must send shutdown");
    let shutdown = wait_for_response(&mut socket, "shutdown").await;
    assert!(
        shutdown["result"]
            .as_str()
            .is_some_and(|result| result.starts_with("OK.")),
        "forceShutdown must return aria2's successful result: {shutdown}"
    );
    assert!(aria2.wait_for_exit(PROCESS_EXIT_TIMEOUT).success());
}
