//! WebSocket tracker announces used during magnet metadata discovery.
//!
//! WebTorrent-compatible trackers accept a JSON announce message over `ws` or
//! `wss`.  They are useful for magnet links that publish WebSocket trackers,
//! but their WebRTC-only offers cannot be consumed by aria2's TCP metadata
//! exchange.  This module therefore returns only TCP-compatible peers and
//! lets the normal DHT/HTTP/UDP fallbacks continue when a tracker has no such
//! peers.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tracing::debug;

use crate::engine::bittorrent::tracker::communication::AnnounceEvent;
use crate::http::socks_connector::NoProxyMatcher;
use crate::http::{HttpConnectProxyTunnel, HttpProxyConfig, ProxyType};
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::DownloadOptions;
use aria2_protocol::bittorrent::tracker::public_list::TrackerFailureKind;
use aria2_protocol::bittorrent::tracker::websocket::{
    WebSocketAnnounceError, WebSocketAnnounceRequest, announce_over_stream, parse_tracker_url,
};

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
    pub(crate) port: u16,
    pub(crate) event: AnnounceEvent,
    pub(crate) options: &'a DownloadOptions,
}

pub(crate) struct AnnounceResponse {
    pub(crate) peers: Vec<SocketAddr>,
    pub(crate) interval: Option<u64>,
    pub(crate) min_interval: Option<u64>,
    pub(crate) seeders: Option<i64>,
    pub(crate) leechers: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct AnnounceError {
    pub(crate) kind: TrackerFailureKind,
    message: String,
}

impl AnnounceError {
    fn new(kind: TrackerFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AnnounceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(crate) async fn announce_with_policy(
    tracker_url: &str,
    announce: AnnounceRequest<'_>,
    policy: &OutboundNetworkPolicy,
) -> Result<AnnounceResponse, AnnounceError> {
    let url = parse_tracker_url(tracker_url).map_err(classify_websocket_announce_error)?;
    let host = url.host_str().ok_or_else(|| {
        AnnounceError::new(
            TrackerFailureKind::Network,
            "WebSocket tracker URL has no host",
        )
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        AnnounceError::new(
            TrackerFailureKind::Network,
            "WebSocket tracker URL has no port",
        )
    })?;
    let timeout = Duration::from_secs(
        announce
            .options
            .bt_tracker_timeout
            .max(announce.options.bt_tracker_connect_timeout)
            .max(1),
    );

    let stream = connect_socket(&url, host, port, announce.options, timeout, policy).await?;
    let event = announce.event.as_event_string();
    let response = announce_over_stream(
        stream,
        url.as_str(),
        &WebSocketAnnounceRequest {
            info_hash: announce.info_hash,
            peer_id: announce.peer_id,
            downloaded: announce.downloaded,
            left: announce.left,
            uploaded: announce.uploaded,
            numwant: announce.numwant,
            port: announce.port,
            event: Some(event),
        },
        timeout,
    )
    .await
    .map_err(classify_websocket_announce_error)?;
    debug!(
        tracker = %tracker_url,
        peers = response.peers.len(),
        "WebSocket tracker announce completed"
    );
    Ok(AnnounceResponse {
        peers: response.peers,
        interval: response.interval,
        min_interval: response.min_interval,
        seeders: response.seeders,
        leechers: response.leechers,
    })
}

fn classify_websocket_announce_error(error: WebSocketAnnounceError) -> AnnounceError {
    let kind = match &error {
        WebSocketAnnounceError::Timeout(_) => TrackerFailureKind::Timeout,
        WebSocketAnnounceError::HandshakeRejected(status)
            if *status >= 500 || matches!(*status, 408 | 425 | 429) =>
        {
            TrackerFailureKind::RemoteTemporary
        }
        WebSocketAnnounceError::HandshakeRejected(_) => TrackerFailureKind::TrackerRejected,
        WebSocketAnnounceError::Protocol(_)
        | WebSocketAnnounceError::Closed
        | WebSocketAnnounceError::Response(
            aria2_protocol::bittorrent::tracker::websocket::WebSocketAnnounceResponseError::Malformed(_),
        ) => TrackerFailureKind::MalformedResponse,
        WebSocketAnnounceError::Response(
            aria2_protocol::bittorrent::tracker::websocket::WebSocketAnnounceResponseError::Rejected(_),
        ) => TrackerFailureKind::TrackerRejected,
        WebSocketAnnounceError::InvalidRequest(_) | WebSocketAnnounceError::Transport(_) => {
            TrackerFailureKind::Network
        }
    };
    AnnounceError::new(kind, error.to_string())
}

async fn connect_socket(
    url: &url::Url,
    target_host: &str,
    target_port: u16,
    options: &DownloadOptions,
    timeout: Duration,
    policy: &OutboundNetworkPolicy,
) -> Result<TcpStream, AnnounceError> {
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
        return tokio::time::timeout(timeout, policy.connect_host(target_host, target_port))
            .await
            .map_err(|_| {
                AnnounceError::new(
                    TrackerFailureKind::Timeout,
                    "WebSocket tracker TCP connection timed out",
                )
            })?
            .map_err(|error| {
                AnnounceError::new(
                    TrackerFailureKind::Network,
                    format!("WebSocket tracker TCP connection failed: {error}"),
                )
            });
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
        HttpProxyConfig::from_proxy_url(proxy_url, target_host.to_string(), target_port).map_err(
            |error| {
                AnnounceError::new(
                    TrackerFailureKind::Network,
                    format!("invalid WebSocket proxy configuration: {error}"),
                )
            },
        )?;
    if config.proxy_type != ProxyType::Http {
        return Err(AnnounceError::new(
            TrackerFailureKind::Network,
            format!(
                "WebSocket tracker proxy must use an HTTP CONNECT proxy; '{}' is {}",
                proxy_url, config.proxy_type
            ),
        ));
    }
    if let Some(username) = credentials.0 {
        config = config.with_credentials(username, credentials.1.unwrap_or_default());
    }
    config.connect_timeout = timeout;
    config.read_timeout = timeout;
    config.write_timeout = timeout;

    tokio::time::timeout(
        timeout,
        HttpConnectProxyTunnel::new(config).connect_with_policy(policy),
    )
    .await
    .map_err(|_| {
        AnnounceError::new(
            TrackerFailureKind::Timeout,
            "WebSocket tracker proxy connection timed out",
        )
    })?
    .map_err(|error| {
        AnnounceError::new(
            TrackerFailureKind::Network,
            format!("WebSocket tracker proxy connection failed: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::tracker::websocket::decode_announce_response;
    use base64::Engine;
    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn parses_json_and_compact_peers() {
        let compact = base64::engine::general_purpose::STANDARD.encode([192, 0, 2, 1, 0x1a, 0xe1]);
        let value = serde_json::json!({
            "action": "announce",
            "peers": [
                {"ip": "192.0.2.2", "port": 6882},
                compact,
            ]
        });
        let encoded = value.to_string();
        let peers = decode_announce_response(&encoded).unwrap().peers;
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
            assert_eq!(
                request["info_hash"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .map(u32::from)
                    .collect::<Vec<_>>(),
                vec![1; 20]
            );
            assert_eq!(
                request["peer_id"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .map(u32::from)
                    .collect::<Vec<_>>(),
                vec![2; 20]
            );
            let response_info_hash = [1u8; 20]
                .iter()
                .copied()
                .map(char::from)
                .collect::<String>();
            websocket
                .send(Message::Text(
                    serde_json::json!({
                        "action": "announce",
                        "info_hash": response_info_hash,
                        "peers": [{"ip": "192.0.2.10", "port": 6881}]
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let peers = announce_with_policy(
            &format!("ws://{address}/announce"),
            AnnounceRequest {
                info_hash: &[1u8; 20],
                peer_id: &[2u8; 20],
                downloaded: 0,
                left: 1,
                uploaded: 0,
                numwant: 50,
                port: 0,
                event: AnnounceEvent::Started,
                options: &DownloadOptions::default(),
            },
            &OutboundNetworkPolicy::direct(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(peers.peers, vec!["192.0.2.10:6881".parse().unwrap()]);
    }
}
