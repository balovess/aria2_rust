//! WebSocket tracker announces used during magnet metadata discovery.
//!
//! WebTorrent-compatible trackers accept a JSON announce message over `ws` or
//! `wss`.  They are useful for magnet links that publish WebSocket trackers,
//! but their WebRTC-only offers cannot be consumed by aria2's TCP metadata
//! exchange.  This module therefore returns only TCP-compatible peers and
//! lets the normal DHT/HTTP/UDP fallbacks continue when a tracker has no such
//! peers.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tracing::debug;

use crate::http::socks_connector::NoProxyMatcher;
use crate::http::{HttpConnectProxyTunnel, HttpProxyConfig, ProxyType};
use crate::request::request_group::DownloadOptions;

/// Announce a magnet lookup to a WebSocket tracker.
///
/// The returned list contains only peers that can be represented as TCP
/// socket addresses.  WebTorrent trackers may return WebRTC `offers` instead;
/// those are intentionally ignored because the BitTorrent peer layer does
/// not implement WebRTC transport.
pub(crate) struct AnnounceRequest<'a> {
    pub(crate) info_hash: &'a [u8; 20],
    pub(crate) peer_id: &'a [u8; 20],
    pub(crate) downloaded: u64,
    pub(crate) left: u64,
    pub(crate) uploaded: u64,
    pub(crate) numwant: u32,
    pub(crate) options: &'a DownloadOptions,
}

pub(crate) async fn announce(
    tracker_url: &str,
    announce: AnnounceRequest<'_>,
) -> Result<Vec<SocketAddr>, String> {
    let url = reqwest::Url::parse(tracker_url)
        .map_err(|error| format!("invalid WebSocket tracker URL: {error}"))?;
    let scheme = url.scheme();
    if !matches!(scheme, "ws" | "wss") {
        return Err(format!("unsupported WebSocket tracker scheme '{scheme}'"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| "WebSocket tracker URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "WebSocket tracker URL has no port".to_string())?;
    let timeout = Duration::from_secs(
        announce
            .options
            .bt_tracker_timeout
            .max(announce.options.bt_tracker_connect_timeout)
            .max(1),
    );

    crate::http::client_pool::ensure_rustls_provider();
    let stream = connect_socket(&url, host, port, announce.options, timeout).await?;
    let ws_request = tracker_url
        .into_client_request()
        .map_err(|error| format!("invalid WebSocket tracker request: {error}"))?;
    let (mut websocket, _) = tokio::time::timeout(
        timeout,
        tokio_tungstenite::client_async_tls_with_config(ws_request, stream, None, None),
    )
    .await
    .map_err(|_| "WebSocket tracker handshake timed out".to_string())?
    .map_err(|error| format!("WebSocket tracker handshake failed: {error}"))?;

    let message = serde_json::json!({
        "action": "announce",
        "info_hash": base64::engine::general_purpose::STANDARD.encode(announce.info_hash),
        "peer_id": base64::engine::general_purpose::STANDARD.encode(announce.peer_id),
        "uploaded": announce.uploaded,
        "downloaded": announce.downloaded,
        "left": announce.left,
        "numwant": announce.numwant,
        "compact": 1,
        "event": "started",
    });
    tokio::time::timeout(timeout, websocket.send(Message::Text(message.to_string())))
        .await
        .map_err(|_| "WebSocket tracker announce timed out while sending".to_string())?
        .map_err(|error| format!("WebSocket tracker announce send failed: {error}"))?;

    let response = tokio::time::timeout(timeout, async {
        let mut response = None;
        while let Some(message) = websocket.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    response = Some(text.to_string());
                    break;
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        response = Some(text);
                        break;
                    }
                }
                Ok(Message::Ping(payload)) => {
                    websocket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok::<_, String>(response)
    })
    .await
    .map_err(|_| "WebSocket tracker announce timed out while receiving".to_string())??;

    let Some(response) = response else {
        return Ok(Vec::new());
    };
    let response: Value = serde_json::from_str(&response)
        .map_err(|error| format!("invalid WebSocket tracker response: {error}"))?;
    if let Some(reason) = response.get("failure reason").and_then(Value::as_str) {
        return Err(format!("WebSocket tracker rejected announce: {reason}"));
    }

    let mut peers = response.get("peers").map(parse_peers).unwrap_or_default();
    if let Some(peers6) = response.get("peers6").and_then(Value::as_str) {
        peers.extend(parse_compact_peers(peers6));
    }
    peers.sort_unstable();
    peers.dedup();
    debug!(tracker = %tracker_url, peers = peers.len(), "WebSocket tracker announce completed");
    Ok(peers)
}

async fn connect_socket(
    url: &reqwest::Url,
    target_host: &str,
    target_port: u16,
    options: &DownloadOptions,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let no_proxy = options
        .no_proxy
        .as_deref()
        .map(NoProxyMatcher::from_env_value)
        .is_some_and(|matcher| matcher.should_bypass_hostname(target_host));

    let proxy = if no_proxy {
        None
    } else if url.scheme() == "ws" {
        options
            .http_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            .or_else(|| {
                options
                    .all_proxy
                    .as_deref()
                    .filter(|proxy| !proxy.is_empty())
            })
    } else {
        options
            .https_proxy
            .as_deref()
            .filter(|proxy| !proxy.is_empty())
            .or_else(|| {
                options
                    .all_proxy
                    .as_deref()
                    .filter(|proxy| !proxy.is_empty())
            })
    };

    let Some(proxy_url) = proxy else {
        return tokio::time::timeout(timeout, TcpStream::connect((target_host, target_port)))
            .await
            .map_err(|_| "WebSocket tracker TCP connection timed out".to_string())?
            .map_err(|error| format!("WebSocket tracker TCP connection failed: {error}"));
    };

    let credentials_selector = if url.scheme() == "ws" {
        "http"
    } else {
        "https"
    };
    let credentials = if (url.scheme() == "ws" && options.http_proxy.as_deref() == Some(proxy_url))
        || (url.scheme() == "wss" && options.https_proxy.as_deref() == Some(proxy_url))
    {
        options.proxy_credentials_for_scheme(credentials_selector)
    } else {
        options.proxy_credentials_for_scheme("all")
    };
    let mut config =
        HttpProxyConfig::from_proxy_url(proxy_url, target_host.to_string(), target_port)
            .map_err(|error| format!("invalid WebSocket proxy configuration: {error}"))?;
    if config.proxy_type != ProxyType::Http {
        return Err(format!(
            "WebSocket tracker proxy must use an HTTP CONNECT proxy; '{}' is {}",
            proxy_url, config.proxy_type
        ));
    }
    if let Some(username) = credentials.0 {
        config = config.with_credentials(username, credentials.1.unwrap_or_default());
    }
    config.connect_timeout = timeout;
    config.read_timeout = timeout;
    config.write_timeout = timeout;

    tokio::time::timeout(timeout, HttpConnectProxyTunnel::new(config).connect())
        .await
        .map_err(|_| "WebSocket tracker proxy connection timed out".to_string())?
        .map_err(|error| format!("WebSocket tracker proxy connection failed: {error}"))
}

fn parse_peers(value: &Value) -> Vec<SocketAddr> {
    match value {
        Value::Array(peers) => peers.iter().flat_map(parse_peer).collect(),
        Value::String(peers) => parse_peer_string(peers),
        _ => Vec::new(),
    }
}

fn parse_peer(value: &Value) -> Vec<SocketAddr> {
    match value {
        Value::Object(peer) => {
            let Some(ip) = peer.get("ip").and_then(Value::as_str) else {
                return Vec::new();
            };
            let Some(port) = peer
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
            else {
                return Vec::new();
            };
            tracker_peer_socket_addr(ip, port).into_iter().collect()
        }
        Value::String(peer) => parse_peer_string(peer),
        _ => Vec::new(),
    }
}

fn parse_peer_string(peer: &str) -> Vec<SocketAddr> {
    if let Ok(address) = peer.parse::<SocketAddr>() {
        return vec![address];
    }
    parse_compact_peers(peer)
}

fn parse_compact_peers(encoded: &str) -> Vec<SocketAddr> {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return Vec::new();
    };
    let mut peers = Vec::new();
    if bytes.len() % 6 == 0 {
        for chunk in bytes.as_chunks::<6>().0 {
            let ip = IpAddr::from([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            peers.push(SocketAddr::new(ip, port));
        }
    } else if bytes.len() % 18 == 0 {
        for chunk in bytes.as_chunks::<18>().0 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&chunk[..16]);
            let ip = IpAddr::from(octets);
            let port = u16::from_be_bytes([chunk[16], chunk[17]]);
            peers.push(SocketAddr::new(ip, port));
        }
    }
    peers
}

fn tracker_peer_socket_addr(ip: &str, port: u16) -> Option<SocketAddr> {
    ip.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_compact_peers() {
        let compact = base64::engine::general_purpose::STANDARD.encode([192, 0, 2, 1, 0x1a, 0xe1]);
        let value = serde_json::json!({
            "peers": [
                {"ip": "192.0.2.2", "port": 6882},
                compact,
            ]
        });
        let mut peers = parse_peers(value.get("peers").unwrap());
        peers.sort_unstable();
        assert_eq!(
            peers,
            vec![
                "192.0.2.1:6881".parse().unwrap(),
                "192.0.2.2:6882".parse().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn announces_to_a_websocket_tracker() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let Some(Ok(Message::Text(request))) = websocket.next().await else {
                panic!("tracker did not receive an announce")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["action"], "announce");
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "peers": [{"ip": "192.0.2.10", "port": 6881}]
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
        });

        let peers = announce(
            &format!("ws://{address}/announce"),
            AnnounceRequest {
                info_hash: &[1u8; 20],
                peer_id: &[2u8; 20],
                downloaded: 0,
                left: 1,
                uploaded: 0,
                numwant: 50,
                options: &DownloadOptions::default(),
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(peers, vec!["192.0.2.10:6881".parse().unwrap()]);
    }
}
