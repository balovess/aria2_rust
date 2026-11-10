//! JSON and compact-peer wire formats for WebSocket tracker announces.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Inputs encoded into a WebTorrent-compatible announce JSON message.
pub struct WebSocketAnnounceRequest<'a> {
    /// Torrent v1 info hash sent in the announce.
    pub info_hash: &'a [u8; 20],
    /// Local BitTorrent peer ID.
    pub peer_id: &'a [u8; 20],
    /// Bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes remaining to complete the torrent.
    pub left: u64,
    /// Bytes uploaded so far.
    pub uploaded: u64,
    /// Requested peer count.
    pub numwant: u32,
    /// Local TCP peer port.
    pub port: u16,
    /// Optional tracker event such as `started` or `completed`.
    pub event: Option<&'a str>,
}

/// Parsed WebSocket tracker announce response.
///
/// WebRTC signaling fields such as `peer_id`, `offer`, `offer_id`, and
/// `answer` are intentionally not exposed; this response contains only
/// tracker statistics and peers with TCP socket addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketAnnounceResponse {
    /// TCP-compatible peers decoded from `peers` and `peers6`.
    pub peers: Vec<SocketAddr>,
    /// Suggested announce interval in seconds.
    pub interval: Option<u64>,
    /// Minimum announce interval in seconds.
    pub min_interval: Option<u64>,
    /// Tracker-reported seed count.
    pub seeders: Option<i64>,
    /// Tracker-reported leecher count.
    pub leechers: Option<i64>,
}

/// Failure while decoding a WebSocket tracker response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebSocketAnnounceResponseError {
    /// The response was not valid JSON.
    #[error("malformed WebSocket tracker response: {0}")]
    Malformed(String),
    /// The tracker rejected the announce with its `failure reason`.
    #[error("WebSocket tracker rejected announce: {0}")]
    Rejected(String),
}

/// Failure while running a WebSocket tracker announce over an established stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebSocketAnnounceError {
    /// The WebSocket URL could not be used to create a handshake request.
    #[error("invalid WebSocket tracker request: {0}")]
    InvalidRequest(String),
    /// A WebSocket operation exceeded its timeout.
    #[error("WebSocket tracker timed out during {0}")]
    Timeout(&'static str),
    /// The tracker rejected the HTTP upgrade with this status.
    #[error("WebSocket tracker rejected handshake with HTTP {0}")]
    HandshakeRejected(u16),
    /// The WebSocket protocol exchange failed.
    #[error("WebSocket tracker protocol error: {0}")]
    Protocol(String),
    /// The WebSocket transport failed.
    #[error("WebSocket tracker transport error: {0}")]
    Transport(String),
    /// The tracker closed the socket before returning an announce response.
    #[error("WebSocket tracker closed without an announce response")]
    Closed,
    /// The announce response was invalid or belonged to another torrent.
    #[error(transparent)]
    Response(#[from] WebSocketAnnounceResponseError),
}

#[derive(Clone, Copy)]
enum CompactPeerFamily {
    V4,
    V6,
}

/// Perform a WebSocket tracker announce over a stream established by the caller.
///
/// The caller owns DNS, socket selection, proxying, and outbound-address policy;
/// this function owns the WebSocket handshake, framing, announce exchange, and
/// response validation.
pub async fn announce_over_stream<S>(
    stream: S,
    tracker_url: &str,
    request: &WebSocketAnnounceRequest<'_>,
    timeout: Duration,
) -> Result<WebSocketAnnounceResponse, WebSocketAnnounceError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ws_request = tracker_url
        .into_client_request()
        .map_err(|error| WebSocketAnnounceError::InvalidRequest(error.to_string()))?;
    let (mut websocket, _) = tokio::time::timeout(
        timeout,
        tokio_tungstenite::client_async_tls_with_config(ws_request, stream, None, None),
    )
    .await
    .map_err(|_| WebSocketAnnounceError::Timeout("handshake"))?
    .map_err(classify_websocket_error)?;

    let message = encode_announce_request(request);
    tokio::time::timeout(timeout, websocket.send(Message::Text(message.into())))
        .await
        .map_err(|_| WebSocketAnnounceError::Timeout("sending"))?
        .map_err(classify_websocket_error)?;

    let response = tokio::time::timeout(timeout, async {
        while let Some(message) = websocket.next().await {
            match message {
                Ok(Message::Text(text)) => return Ok(Some(text.to_string())),
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        return Ok(Some(text));
                    }
                }
                Ok(Message::Ping(payload)) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(classify_websocket_error)?,
                Ok(Message::Close(_)) => return Ok(None),
                Ok(_) => {}
                Err(error) => return Err(classify_websocket_error(error)),
            }
        }
        Ok(None)
    })
    .await
    .map_err(|_| WebSocketAnnounceError::Timeout("receiving"))??
    .ok_or(WebSocketAnnounceError::Closed)?;

    decode_announce_response_for_request(&response, request.info_hash)
        .map_err(WebSocketAnnounceError::Response)
}

fn classify_websocket_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> WebSocketAnnounceError {
    use tokio_tungstenite::tungstenite::Error;

    match error {
        Error::Http(response) => {
            WebSocketAnnounceError::HandshakeRejected(response.status().as_u16())
        }
        Error::Protocol(error) => WebSocketAnnounceError::Protocol(error.to_string()),
        error => WebSocketAnnounceError::Transport(error.to_string()),
    }
}

/// Encode a WebTorrent-compatible JSON announce request.
pub fn encode_announce_request(request: &WebSocketAnnounceRequest<'_>) -> String {
    let mut message = serde_json::json!({
        "action": "announce",
        "info_hash": binary_string(request.info_hash),
        "peer_id": binary_string(request.peer_id),
        "uploaded": request.uploaded,
        "downloaded": request.downloaded,
        "left": request.left,
        "numwant": request.numwant,
        "port": request.port,
        "compact": 1,
    });
    if let Some(event) = request.event.filter(|event| !event.is_empty()) {
        message["event"] = Value::String(event.to_owned());
    }
    message.to_string()
}

/// Decode a WebTorrent-compatible JSON announce response.
///
/// Malformed peer entries are ignored, matching the established tracker
/// behavior. A tracker `failure reason` is returned as `Rejected`.
pub fn decode_announce_response(
    encoded: &str,
) -> Result<WebSocketAnnounceResponse, WebSocketAnnounceResponseError> {
    decode_announce_response_inner(encoded, None)
}

/// Decode an announce response and verify that it belongs to the requested torrent.
pub fn decode_announce_response_for_request(
    encoded: &str,
    expected_info_hash: &[u8; 20],
) -> Result<WebSocketAnnounceResponse, WebSocketAnnounceResponseError> {
    decode_announce_response_inner(encoded, Some(expected_info_hash))
}

fn decode_announce_response_inner(
    encoded: &str,
    expected_info_hash: Option<&[u8; 20]>,
) -> Result<WebSocketAnnounceResponse, WebSocketAnnounceResponseError> {
    let response: Value = serde_json::from_str(encoded)
        .map_err(|error| WebSocketAnnounceResponseError::Malformed(error.to_string()))?;
    let response = response.as_object().ok_or_else(|| {
        WebSocketAnnounceResponseError::Malformed("response must be a JSON object".to_owned())
    })?;
    if response.get("action").and_then(Value::as_str) != Some("announce") {
        return Err(WebSocketAnnounceResponseError::Malformed(
            "response action must be `announce`".to_owned(),
        ));
    }
    if let Some(expected_info_hash) = expected_info_hash {
        let actual_info_hash = response
            .get("info_hash")
            .and_then(Value::as_str)
            .and_then(parse_binary_info_hash)
            .ok_or_else(|| {
                WebSocketAnnounceResponseError::Malformed(
                    "response must contain a 20-byte binary info_hash".to_owned(),
                )
            })?;
        if &actual_info_hash != expected_info_hash {
            return Err(WebSocketAnnounceResponseError::Malformed(
                "response info_hash does not match the announce request".to_owned(),
            ));
        }
    }
    if let Some(reason) = response.get("failure reason").and_then(Value::as_str) {
        return Err(WebSocketAnnounceResponseError::Rejected(reason.to_owned()));
    }

    let mut peers = response
        .get("peers")
        .map(|peers| parse_peers(peers, CompactPeerFamily::V4))
        .unwrap_or_default();
    if let Some(peers6) = response.get("peers6").and_then(Value::as_str) {
        peers.extend(parse_compact_peers(peers6, CompactPeerFamily::V6));
    }
    peers.sort_unstable();
    peers.dedup();

    Ok(WebSocketAnnounceResponse {
        peers,
        interval: response
            .get("interval")
            .and_then(Value::as_u64)
            .filter(|seconds| *seconds > 0),
        min_interval: response
            .get("min interval")
            .and_then(Value::as_u64)
            .filter(|seconds| *seconds > 0),
        seeders: response
            .get("complete")
            .and_then(Value::as_i64)
            .filter(|count| *count >= 0),
        leechers: response
            .get("incomplete")
            .and_then(Value::as_i64)
            .filter(|count| *count >= 0),
    })
}

fn parse_binary_info_hash(value: &str) -> Option<[u8; 20]> {
    let mut bytes = [0; 20];
    let mut chars = value.chars();
    for byte in &mut bytes {
        *byte = u8::try_from(u32::from(chars.next()?)).ok()?;
    }
    chars.next().is_none().then_some(bytes)
}

fn parse_peers(value: &Value, family: CompactPeerFamily) -> Vec<SocketAddr> {
    match value {
        Value::Array(peers) => peers
            .iter()
            .flat_map(|peer| parse_peer(peer, family))
            .collect(),
        Value::String(peers) => parse_peer_string(peers, family),
        _ => Vec::new(),
    }
}

fn parse_peer(value: &Value, family: CompactPeerFamily) -> Vec<SocketAddr> {
    match value {
        Value::Object(peer) => {
            let Some(ip) = peer.get("ip").and_then(Value::as_str) else {
                return Vec::new();
            };
            let Some(port) = peer
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0)
            else {
                return Vec::new();
            };
            tracker_peer_socket_addr(ip, port).into_iter().collect()
        }
        Value::String(peer) => parse_peer_string(peer, family),
        _ => Vec::new(),
    }
}

fn parse_peer_string(peer: &str, family: CompactPeerFamily) -> Vec<SocketAddr> {
    if let Ok(address) = peer.parse::<SocketAddr>() {
        return (address.port() > 0)
            .then_some(address)
            .into_iter()
            .collect();
    }
    parse_compact_peers(peer, family)
}

fn parse_compact_peers(encoded: &str, family: CompactPeerFamily) -> Vec<SocketAddr> {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Vec::new();
    };
    let mut peers = Vec::new();
    match family {
        CompactPeerFamily::V4 if bytes.len().is_multiple_of(6) => {
            for chunk in bytes.as_chunks::<6>().0 {
                let ip = IpAddr::from([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                if port > 0 {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
        CompactPeerFamily::V6 if bytes.len().is_multiple_of(18) => {
            for chunk in bytes.as_chunks::<18>().0 {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&chunk[..16]);
                let ip = IpAddr::from(octets);
                let port = u16::from_be_bytes([chunk[16], chunk[17]]);
                if port > 0 {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
        _ => {}
    }
    peers
}

fn tracker_peer_socket_addr(ip: &str, port: u16) -> Option<SocketAddr> {
    ip.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

fn binary_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_announce_fields_and_omits_empty_event() {
        let encoded = encode_announce_request(&WebSocketAnnounceRequest {
            info_hash: &[0x11; 20],
            peer_id: &[0x22; 20],
            downloaded: 12,
            left: 34,
            uploaded: 56,
            numwant: 7,
            port: 6881,
            event: Some(""),
        });
        let message: Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(message["action"], "announce");
        assert_eq!(message["downloaded"], 12);
        assert_eq!(message["left"], 34);
        assert_eq!(message["uploaded"], 56);
        assert_eq!(message["numwant"], 7);
        assert_eq!(message["port"], 6881);
        assert!(message.get("event").is_none());
        assert_eq!(message["info_hash"], binary_string(&[0x11; 20]));
        assert_eq!(message["peer_id"], binary_string(&[0x22; 20]));
    }

    #[test]
    fn decodes_ipv4_and_ipv6_compact_peers_by_response_field() {
        let peers_v4 = base64::engine::general_purpose::STANDARD.encode([192, 0, 2, 1, 0x1a, 0xe1]);
        let mut peers_v6 = [0u8; 18];
        peers_v6[..16].copy_from_slice(
            &"2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        peers_v6[16..].copy_from_slice(&6882u16.to_be_bytes());
        let peers_v6 = base64::engine::general_purpose::STANDARD.encode(peers_v6);
        let encoded = serde_json::json!({
            "action": "announce",
            "peers": peers_v4,
            "peers6": peers_v6
        })
        .to_string();

        let response = decode_announce_response(&encoded).unwrap();
        assert_eq!(response.peers.len(), 2);
        assert!(response.peers.contains(&"192.0.2.1:6881".parse().unwrap()));
        assert!(
            response
                .peers
                .contains(&"[2001:db8::1]:6882".parse().unwrap())
        );
    }

    #[test]
    fn decodes_mixed_object_and_compact_peer_entries() {
        let compact = base64::engine::general_purpose::STANDARD.encode([192, 0, 2, 1, 0x1a, 0xe1]);
        let encoded = serde_json::json!({
            "action": "announce",
            "peers": [
                {"ip": "192.0.2.2", "port": 6882},
                compact,
            ],
            "interval": 120,
            "min interval": 60,
            "complete": 3,
            "incomplete": 4,
        })
        .to_string();

        let response = decode_announce_response(&encoded).unwrap();
        assert_eq!(response.peers.len(), 2);
        assert_eq!(response.interval, Some(120));
        assert_eq!(response.min_interval, Some(60));
        assert_eq!(response.seeders, Some(3));
        assert_eq!(response.leechers, Some(4));
    }

    #[test]
    fn ignores_webrtc_signaling_fields_without_treating_them_as_tcp_peers() {
        let encoded = serde_json::json!({
            "action": "announce",
            "interval": 120,
            "peer_id": binary_string(&[0x22; 20]),
            "offer": {"type": "offer", "sdp": "opaque signaling data"},
            "offer_id": "offer-1",
            "answer": {"type": "answer", "sdp": "opaque signaling data"},
        })
        .to_string();

        let response = decode_announce_response(&encoded).unwrap();

        assert!(response.peers.is_empty());
        assert_eq!(response.interval, Some(120));
    }

    #[test]
    fn ignores_compact_peer_data_with_invalid_lengths() {
        let peers_v4 = base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4, 5]);
        let peers_v6 = base64::engine::general_purpose::STANDARD.encode([0; 17]);
        let encoded = serde_json::json!({
            "action": "announce",
            "peers": peers_v4,
            "peers6": peers_v6
        })
        .to_string();

        assert!(decode_announce_response(&encoded).unwrap().peers.is_empty());
    }

    #[test]
    fn validates_response_info_hash_against_announce_request() {
        let expected_info_hash = [0x31; 20];
        let encoded = serde_json::json!({
            "action": "announce",
            "info_hash": binary_string(&expected_info_hash),
        })
        .to_string();

        assert!(decode_announce_response_for_request(&encoded, &expected_info_hash).is_ok());
        assert!(matches!(
            decode_announce_response_for_request(&encoded, &[0x32; 20]),
            Err(WebSocketAnnounceResponseError::Malformed(_))
        ));

        let missing_hash = r#"{"action":"announce"}"#;
        assert!(matches!(
            decode_announce_response_for_request(missing_hash, &expected_info_hash),
            Err(WebSocketAnnounceResponseError::Malformed(_))
        ));
    }

    #[test]
    fn reports_malformed_and_rejected_responses() {
        assert!(matches!(
            decode_announce_response("{"),
            Err(WebSocketAnnounceResponseError::Malformed(_))
        ));
        assert!(matches!(
            decode_announce_response("[]"),
            Err(WebSocketAnnounceResponseError::Malformed(_))
        ));
        assert!(matches!(
            decode_announce_response(r#"{"action":"scrape"}"#),
            Err(WebSocketAnnounceResponseError::Malformed(_))
        ));
        let error = decode_announce_response(r#"{"action":"announce","failure reason":"denied"}"#)
            .unwrap_err();
        assert_eq!(
            error,
            WebSocketAnnounceResponseError::Rejected("denied".to_owned())
        );
    }

    #[test]
    fn ignores_zero_ports_and_invalid_tracker_statistics() {
        let compact_v4 = base64::engine::general_purpose::STANDARD
            .encode([192, 0, 2, 1, 0, 0, 192, 0, 2, 2, 0x1a, 0xe1]);
        let encoded = serde_json::json!({
            "action": "announce",
            "peers": [
                {"ip": "192.0.2.3", "port": 0},
                {"ip": "192.0.2.4", "port": 65536},
                "192.0.2.5:0",
                compact_v4,
            ],
            "interval": 0,
            "min interval": 0,
            "complete": -1,
            "incomplete": -2,
        })
        .to_string();

        let response = decode_announce_response(&encoded).unwrap();

        assert_eq!(response.peers, vec!["192.0.2.2:6881".parse().unwrap()]);
        assert_eq!(response.interval, None);
        assert_eq!(response.min_interval, None);
        assert_eq!(response.seeders, None);
        assert_eq!(response.leechers, None);
    }
}
