//! BT peer connection manager and initialization path.
//!
//! This module manages the interaction with BitTorrent peers, including:
//! - Connection establishment (plain and encrypted)
//! - BitTorrent handshake and peer identity checks
//! - Handing the established stream to its long-lived Swarm actor
//!
//! # Compatibility Reference
//!
//! Protocol behavior is checked against original aria2's:
//! - `src/PeerInteractionCommand.h/.cc` — Peer connection lifecycle command
//! - `src/PeerConnection.cc/h` — Peer connection management
//!   This module does not own post-handshake protocol I/O; the peer actor does.

mod types;

pub use types::{BtPeerConnectionOptions, BtPeerCryptoPolicy, PeerConnectionResult};

// ======================================================================
// BtPeerInteraction — peer connection lifecycle manager
// ======================================================================

use futures::stream::{self, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorStartup};
use crate::error::Result;
use crate::network::OutboundNetworkPolicy;
use tracing::{debug, info};

/// Maximum time to wait for additional peers after the first successful connection.
pub(crate) const PEER_CONNECT_SETTLE_TIME: std::time::Duration = std::time::Duration::from_secs(1);

/// BT Peer Interaction Manager
///
/// Handles the lifecycle of peer connections from initial connection
/// through the handshake phase until they're ready for data transfer.
pub struct BtPeerInteraction;

impl BtPeerInteraction {
    /// Connect to multiple peers with automatic fallback strategies
    ///
    /// Attempts to connect to all provided peer addresses using:
    /// 1. MSE encryption if required or forced
    /// 2. Plain connection as fallback
    ///
    /// Socket ownership passes to the peer actor immediately after handshake.
    ///
    /// # Arguments
    /// * `peer_addrs` - List of peer addresses to connect to
    /// * `info_hash_raw` - Torrent info hash for handshake
    /// * `num_pieces` - Total number of pieces (for bitfield size)
    /// * `require_crypto` - Whether to require encrypted connections
    /// * `force_encrypt` - Whether to force encryption (fallback to plain)
    ///
    /// # Returns
    /// * `PeerConnectionResult` containing connected peers and failure count
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_to_peers(
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        piece_length: u32,
        total_length: u64,
        connection_options: &BtPeerConnectionOptions,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<PeerConnectionResult> {
        info!("[BT] Connecting to {} peers...", peer_addrs.len());

        // A peer can legitimately delay Unchoke while the tracker has already
        // supplied other usable peers. Establish each candidate independently
        // so one slow peer cannot prevent the piece scheduler from starting.
        let mut results = stream::iter(peer_addrs.iter().cloned())
            .map(|addr| {
                let utp_socket = utp_socket.clone();
                async move {
                    debug!("[BT] Connecting to peer {}:{}", addr.ip, addr.port);
                    let result = Self::connect_peer_ready(
                        &addr,
                        info_hash_raw,
                        connection_options,
                        num_pieces,
                        piece_length,
                        total_length,
                        utp_socket.clone(),
                        policy,
                    )
                    .await;
                    (addr, result)
                }
            })
            .buffer_unordered(peer_addrs.len().max(1));

        let mut active_connections = Vec::with_capacity(peer_addrs.len());
        let mut failed_count = 0usize;
        let mut settle_deadline = None;
        loop {
            let next = if let Some(deadline) = settle_deadline {
                tokio::select! {
                    biased;
                    result = results.next() => result,
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            } else {
                results.next().await
            };
            let Some((addr, result)) = next else {
                break;
            };
            match result {
                Ok(conn) => {
                    active_connections.push(conn);
                    settle_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + PEER_CONNECT_SETTLE_TIME
                    });
                }
                Err(e) => {
                    debug!(
                        "[BT] Failed to connect peer {}:{}: {}",
                        addr.ip, addr.port, e
                    );
                    failed_count += 1;
                }
            }
        }

        info!("[BT] Active connections: {}", active_connections.len());

        Ok(PeerConnectionResult {
            connections: active_connections,
            failed_count,
        })
    }

    /// Establish and initialize one peer using the same crypto and protocol path
    /// as the initial peer batch.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_peer_ready(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        connection_options: &BtPeerConnectionOptions,
        num_pieces: u32,
        piece_length: u32,
        total_length: u64,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<BtPeerConn> {
        let mut conn =
            Self::connect_single_peer(addr, info_hash_raw, connection_options, utp_socket, policy)
                .await?;
        conn.set_timeouts(
            connection_options.keep_alive_interval,
            connection_options.peer_timeout,
        );
        conn.sync_peer_identity();
        conn.allocate_session_resource(piece_length, num_pieces, total_length);
        info!(
            "[BT] Connected to peer {}:{} (encrypted={}, piece_length={}, total_length={})",
            addr.ip,
            addr.port,
            conn.is_encrypted(),
            piece_length,
            total_length
        );
        conn.actor_startup = Some(PeerActorStartup {
            peer_agent: connection_options.peer_agent.clone(),
            listen_port: connection_options.listen_port,
            allowed_fast: if conn.is_fast_extension_enabled() {
                aria2_protocol::bittorrent::fast_set::compute_fast_set(
                    &addr.ip,
                    num_pieces,
                    info_hash_raw,
                    10,
                )
            } else {
                Vec::new()
            },
        });
        Ok(conn)
    }

    /// Connect to a single peer with encryption fallback logic
    async fn connect_single_peer(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        connection_options: &BtPeerConnectionOptions,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<BtPeerConn> {
        if connection_options.enable_utp && !connection_options.crypto.require_mse {
            match addr.to_socket_addr() {
                Ok(endpoint) => {
                    let utp_result = BtPeerConn::connect_utp_with_policy(
                        endpoint,
                        info_hash_raw,
                        connection_options.hybrid_info_hash_v2.as_ref(),
                        crate::engine::bittorrent::peer::connection::UtpConnectionOptions {
                            local_peer_id: connection_options.local_peer_id,
                            timeout: connection_options.connection_timeout,
                            listen_port: connection_options.utp_listen_port,
                            shared_socket: utp_socket,
                            dht_enabled: connection_options.dht_enabled,
                        },
                        policy,
                    )
                    .await;
                    match utp_result {
                        Ok(conn) => {
                            debug!("[BT] Connected to peer {}:{} over uTP", addr.ip, addr.port);
                            return Ok(conn);
                        }
                        Err(error) => {
                            debug!(
                                "[BT] uTP connection to {}:{} failed, trying TCP: {}",
                                addr.ip, addr.port, error
                            );
                        }
                    }
                }
                Err(error) => {
                    debug!(
                        "[BT] Skipping uTP for peer {}:{} because it is not a numeric IP address: {}",
                        addr.ip, addr.port, error
                    );
                }
            }
        }

        if connection_options.crypto.require_mse {
            // Try MSE encrypted connection
            BtPeerConn::connect_mse_with_policy(
                addr,
                info_hash_raw,
                connection_options.hybrid_info_hash_v2.as_ref(),
                aria2_protocol::bittorrent::peer::mse::MseConnectionOptions {
                    force_encryption: connection_options.crypto.force_encryption,
                    prefer_encryption: connection_options.crypto.prefer_encryption,
                    local_peer_id: connection_options.local_peer_id,
                    timeout: connection_options.connection_timeout,
                    dht_enabled: connection_options.dht_enabled,
                },
                policy,
            )
            .await
        } else {
            // Try MSE first, fall back to plain
            let mse_result = BtPeerConn::connect_mse_with_policy(
                addr,
                info_hash_raw,
                connection_options.hybrid_info_hash_v2.as_ref(),
                aria2_protocol::bittorrent::peer::mse::MseConnectionOptions {
                    force_encryption: connection_options.crypto.force_encryption,
                    prefer_encryption: connection_options.crypto.prefer_encryption,
                    local_peer_id: connection_options.local_peer_id,
                    timeout: connection_options.connection_timeout,
                    dht_enabled: connection_options.dht_enabled,
                },
                policy,
            )
            .await;
            match mse_result {
                Ok(conn) => Ok(conn),
                Err(_) => {
                    debug!("[BT] MSE failed, trying plain connection");
                    BtPeerConn::connect_plain_with_policy(
                        addr,
                        info_hash_raw,
                        connection_options.hybrid_info_hash_v2.as_ref(),
                        &connection_options.local_peer_id,
                        connection_options.connection_timeout,
                        connection_options.dht_enabled,
                        policy,
                    )
                    .await
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BtPeerConnectionOptions, BtPeerCryptoPolicy, BtPeerInteraction};
    use crate::network::OutboundNetworkPolicy;
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;
    use std::time::Duration;

    #[tokio::test]
    async fn a_slow_peer_does_not_hold_a_ready_peer_batch_open() {
        let info_hash = [7; 20];
        let remote_peer_id = [8; 20];
        let good_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good_listener.local_addr().unwrap();
        let good_server = tokio::spawn(async move {
            loop {
                let (stream, _) = good_listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete(remote_peer_id, None, false).await {
                        return connection;
                    }
                }
            }
        });

        let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let slow_server = tokio::spawn(async move {
            while let Ok((stream, _)) = slow_listener.accept().await {
                tokio::spawn(async move {
                    let _stream = stream;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                });
            }
        });

        let options = BtPeerConnectionOptions {
            crypto: BtPeerCryptoPolicy::default(),
            connection_timeout: Duration::from_secs(2),
            keep_alive_interval: Duration::from_secs(120),
            peer_timeout: Duration::from_secs(60),
            local_peer_id: [9; 20],
            peer_agent: "aria2-rust-test".to_string(),
            enable_utp: false,
            utp_listen_port: None,
            dht_enabled: false,
            listen_port: None,
            hybrid_info_hash_v2: None,
        };
        let peers = [
            PeerAddr::new(&good_addr.ip().to_string(), good_addr.port()),
            PeerAddr::new(&slow_addr.ip().to_string(), slow_addr.port()),
        ];

        let result = tokio::time::timeout(
            Duration::from_millis(1500),
            BtPeerInteraction::connect_to_peers(
                &peers,
                &info_hash,
                1,
                16 * 1024,
                3,
                &options,
                None,
                &OutboundNetworkPolicy::direct(),
            ),
        )
        .await
        .expect("the ready peer should be returned before the slow peer times out")
        .unwrap();

        assert_eq!(result.connections.len(), 1);
        drop(result);
        good_server.abort();
        slow_server.abort();
    }

    #[tokio::test]
    async fn later_peer_success_does_not_extend_the_settle_deadline() {
        let info_hash = [7; 20];
        let remote_peer_id = [8; 20];
        let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_addr = first_listener.local_addr().unwrap();
        let first_server = tokio::spawn(async move {
            loop {
                let (stream, _) = first_listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete(remote_peer_id, None, false).await {
                        return connection;
                    }
                }
            }
        });

        let delayed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let delayed_addr = delayed_listener.local_addr().unwrap();
        let delayed_server = tokio::spawn(async move {
            loop {
                let (stream, _) = delayed_listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete([10; 20], None, false).await {
                        return connection;
                    }
                }
                // The first connection is the MSE probe. Delay accepting the
                // plaintext retry so this peer becomes ready well inside the
                // first peer's one-second settle window.
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });

        let pending_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pending_addr = pending_listener.local_addr().unwrap();
        let pending_server = tokio::spawn(async move {
            while let Ok((stream, _)) = pending_listener.accept().await {
                tokio::spawn(async move {
                    let _stream = stream;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                });
            }
        });

        let options = BtPeerConnectionOptions {
            crypto: BtPeerCryptoPolicy::default(),
            connection_timeout: Duration::from_secs(2),
            keep_alive_interval: Duration::from_secs(120),
            peer_timeout: Duration::from_secs(60),
            local_peer_id: [9; 20],
            peer_agent: "aria2-rust-test".to_string(),
            enable_utp: false,
            utp_listen_port: None,
            dht_enabled: false,
            listen_port: None,
            hybrid_info_hash_v2: None,
        };
        let peers = [
            PeerAddr::new(&first_addr.ip().to_string(), first_addr.port()),
            PeerAddr::new(&delayed_addr.ip().to_string(), delayed_addr.port()),
            PeerAddr::new(&pending_addr.ip().to_string(), pending_addr.port()),
        ];

        let result = tokio::time::timeout(
            Duration::from_millis(1250),
            BtPeerInteraction::connect_to_peers(
                &peers,
                &info_hash,
                1,
                16 * 1024,
                3,
                &options,
                None,
                &OutboundNetworkPolicy::direct(),
            ),
        )
        .await
        .expect("a later success must not renew the first peer's settle deadline")
        .unwrap();

        assert_eq!(result.connections.len(), 2);
        drop(result);
        first_server.abort();
        delayed_server.abort();
        pending_server.abort();
    }

    #[tokio::test]
    async fn ipv6_peer_with_utp_enabled_falls_back_to_tcp() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let info_hash = [7; 20];
        let remote_peer_id = [8; 20];
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                {
                    if let Ok(connection) = incoming.complete(remote_peer_id, None, false).await {
                        return connection;
                    }
                }
            }
        });

        let options = BtPeerConnectionOptions {
            crypto: BtPeerCryptoPolicy::default(),
            connection_timeout: Duration::from_millis(150),
            keep_alive_interval: Duration::from_secs(120),
            peer_timeout: Duration::from_secs(60),
            local_peer_id: [9; 20],
            peer_agent: "aria2-rust-test".to_string(),
            enable_utp: true,
            utp_listen_port: None,
            dht_enabled: false,
            listen_port: None,
            hybrid_info_hash_v2: None,
        };

        let result = BtPeerInteraction::connect_single_peer(
            &PeerAddr::new(&endpoint.ip().to_string(), endpoint.port()),
            &info_hash,
            &options,
            None,
            &OutboundNetworkPolicy::direct(),
        )
        .await;
        if let Err(error) = result {
            server.abort();
            panic!("IPv6 peer connection should fall back from uTP to TCP: {error}");
        }

        server.abort();
    }
}
