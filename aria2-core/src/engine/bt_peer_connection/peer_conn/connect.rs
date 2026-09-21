//! Connection constructors for [`BtPeerConn`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::engine::peer_stats::PeerStats;
use crate::error::{Aria2Error, FatalError, Result};
use crate::network::OutboundNetworkPolicy;

use super::super::types::{ConnectionType, SendBuffer};
use super::super::utp_connection::UtpPeerConnection;
use super::{BtPeerConn, InnerConnection, KEEPALIVE_INTERVAL_SECS, PEER_TIMEOUT_SECS};

#[derive(Clone, Copy)]
pub struct MseConnectionOptions {
    pub force_encryption: bool,
    pub prefer_encryption: bool,
    pub local_peer_id: [u8; 20],
    pub timeout: std::time::Duration,
    pub dht_enabled: bool,
}

#[derive(Clone)]
pub struct UtpConnectionOptions {
    pub local_peer_id: [u8; 20],
    pub timeout: std::time::Duration,
    pub listen_port: Option<u16>,
    pub shared_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
    pub dht_enabled: bool,
}

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Connection constructors
    // -----------------------------------------------------------------------

    /// Connect via MSE using the task's peer identity and connection timeout.
    pub async fn connect_mse_with_options(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: MseConnectionOptions,
    ) -> Result<Self> {
        Self::connect_mse_with_policy(
            addr,
            info_hash_v1,
            info_hash_v2,
            options,
            &OutboundNetworkPolicy::direct(),
        )
        .await
    }

    /// Connect through the process-wide outbound network policy.
    pub async fn connect_mse_with_policy(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: MseConnectionOptions,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        Self::connect_mse_with_hashes(addr, info_hash_v1, info_hash_v2, options, policy).await
    }

    async fn connect_mse_with_hashes(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: MseConnectionOptions,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        let socket_addr = addr
            .to_socket_addr()
            .map_err(|error| Aria2Error::Fatal(FatalError::Config(error.to_string())))?;
        let stream = tokio::time::timeout(options.timeout, policy.connect(socket_addr))
            .await
            .map_err(|_| Aria2Error::Fatal(FatalError::Config("peer connection timeout".into())))?
            .map_err(|error| Aria2Error::Fatal(FatalError::Config(error.to_string())))?;
        let connection =
            aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_stream(
                stream,
                info_hash_v1,
                info_hash_v2,
                aria2_protocol::bittorrent::peer::encrypted_connection::MseConnectionOptions {
                    force_encryption: options.force_encryption,
                    prefer_encryption: options.prefer_encryption,
                    local_peer_id: options.local_peer_id,
                    timeout: options.timeout,
                    dht_enabled: options.dht_enabled,
                },
            )
            .await;
        match connection {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Encrypted(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
                    source: crate::request::request_group::BtPeerSource::Unknown,
                    local_peer: false,
                    disconnected_gracefully: false,
                    seeder: false,
                    first_contact_time: now,
                    connection_type: ConnectionType::Tcp,
                    allowed_fast: HashSet::new(),
                    session_resource: None,
                    send_buffer: SendBuffer::new(),
                    last_keepalive_sent: now,
                    last_message_received: now,
                    keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
                    peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
                    stats: PeerStats::new(
                        [0u8; 20],
                        std::net::SocketAddr::new(
                            addr.ip.parse().map_err(|_| {
                                Aria2Error::Fatal(FatalError::Config(format!(
                                    "Invalid peer IP address: {}",
                                    addr.ip
                                )))
                            })?,
                            addr.port,
                        ),
                    ),
                    pending_pex_peers: Vec::new(),
                    pex_enabled: true,
                    upload_state: None,
                    upload_progress: None,
                })
            }
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via plain TCP using the task's peer identity and timeout.
    pub async fn connect_plain_with_options(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
    ) -> Result<Self> {
        Self::connect_plain_with_hashes(
            addr,
            info_hash_v1,
            info_hash_v2,
            local_peer_id,
            timeout,
            dht_enabled,
            &OutboundNetworkPolicy::direct(),
        )
        .await
    }

    pub async fn connect_plain_with_policy(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        Self::connect_plain_with_hashes(
            addr,
            info_hash_v1,
            info_hash_v2,
            local_peer_id,
            timeout,
            dht_enabled,
            policy,
        )
        .await
    }

    async fn connect_plain_with_hashes(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        let socket_addr = addr
            .to_socket_addr()
            .map_err(|error| Aria2Error::Fatal(FatalError::Config(error.to_string())))?;
        let result = match tokio::time::timeout(timeout, policy.connect(socket_addr)).await {
            Ok(Ok(stream)) => {
                aria2_protocol::bittorrent::peer::connection::PeerConnection::connect_with_stream(
                    stream,
                    socket_addr,
                    info_hash_v1,
                    info_hash_v2,
                    local_peer_id,
                    timeout,
                    dht_enabled,
                )
                .await
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(format!("Peer connection timeout: {socket_addr}")),
        };
        match result {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Plain(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
                    source: crate::request::request_group::BtPeerSource::Unknown,
                    local_peer: false,
                    disconnected_gracefully: false,
                    seeder: false,
                    first_contact_time: now,
                    connection_type: ConnectionType::Tcp,
                    allowed_fast: HashSet::new(),
                    session_resource: None,
                    send_buffer: SendBuffer::new(),
                    last_keepalive_sent: now,
                    last_message_received: now,
                    keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
                    peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
                    stats: PeerStats::new(
                        [0u8; 20],
                        std::net::SocketAddr::new(
                            addr.ip.parse().map_err(|_| {
                                Aria2Error::Fatal(FatalError::Config(format!(
                                    "Invalid peer IP address: {}",
                                    addr.ip
                                )))
                            })?,
                            addr.port,
                        ),
                    ),
                    pending_pex_peers: Vec::new(),
                    pex_enabled: true,
                    upload_state: None,
                    upload_progress: None,
                })
            }
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Wrap an already handshaken incoming TCP peer.
    pub(crate) fn from_incoming_plain(
        conn: aria2_protocol::bittorrent::peer::connection::PeerConnection,
        endpoint: std::net::SocketAddr,
    ) -> Self {
        let now = Instant::now();
        let peer_id = conn.remote_peer_id().copied();
        Self {
            inner: InnerConnection::Plain(conn),
            ip_addr: endpoint.ip().to_string(),
            port: endpoint.port(),
            peer_id,
            incoming: true,
            source: crate::request::request_group::BtPeerSource::Incoming,
            local_peer: endpoint.ip().is_loopback()
                || matches!(endpoint.ip(), std::net::IpAddr::V4(address) if address.is_private()),
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
            stats: PeerStats::new(peer_id.unwrap_or([0u8; 20]), endpoint),
            pending_pex_peers: Vec::new(),
            pex_enabled: true,
            upload_state: None,
            upload_progress: None,
        }
    }

    /// Wrap an incoming peer after the shared listener completed MSE.
    pub(crate) fn from_incoming_encrypted(
        conn: aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection,
        endpoint: std::net::SocketAddr,
    ) -> Self {
        let now = Instant::now();
        let peer_id = conn.remote_peer_id().copied();
        Self {
            inner: InnerConnection::Encrypted(conn),
            ip_addr: endpoint.ip().to_string(),
            port: endpoint.port(),
            peer_id,
            incoming: true,
            source: crate::request::request_group::BtPeerSource::Incoming,
            local_peer: endpoint.ip().is_loopback()
                || matches!(endpoint.ip(), std::net::IpAddr::V4(address) if address.is_private()),
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
            stats: PeerStats::new(peer_id.unwrap_or([0u8; 20]), endpoint),
            pending_pex_peers: Vec::new(),
            pex_enabled: true,
            upload_state: None,
            upload_progress: None,
        }
    }

    /// Create a stub connection for unit testing.
    ///
    /// This creates a loopback TCP connection pair. The returned
    /// `BtPeerConn` is not actually connected to any real peer,
    /// but has enough structure for unit tests that need to inspect
    /// or modify fields like `session_resource`, `allowed_fast`, etc.
    #[cfg(test)]
    pub fn new_stub(info_hash: &[u8; 20]) -> Self {
        let now = Instant::now();
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Create a loopback connection pair. We only need one side
        // for the stub; the other is dropped.
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let stream = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local_addr = listener.local_addr().unwrap();
            let (_, stream) = tokio::join!(
                tokio::net::TcpStream::connect(local_addr),
                listener.accept()
            );
            stream.unwrap().0
        });

        let peer_conn =
            aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
                stream, [0u8; 20], false, false,
            );

        Self {
            inner: InnerConnection::Plain(peer_conn),
            ip_addr: "127.0.0.1".to_string(),
            port: 0,
            peer_id: Some(*info_hash),
            incoming: false,
            source: crate::request::request_group::BtPeerSource::Unknown,
            local_peer: true,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
            pex_enabled: true,
            upload_state: None,
            upload_progress: None,
        }
    }

    /// Connect via uTP using the task's peer identity, timeout, and shared
    /// socket when one is available.
    pub async fn connect_utp_with_options(
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: UtpConnectionOptions,
    ) -> Result<Self> {
        Self::connect_utp_with_policy(
            addr,
            info_hash_v1,
            info_hash_v2,
            options,
            &OutboundNetworkPolicy::direct(),
        )
        .await
    }

    pub async fn connect_utp_with_policy(
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: UtpConnectionOptions,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        Self::connect_utp_with_hashes(addr, info_hash_v1, info_hash_v2, options, policy).await
    }

    async fn connect_utp_with_hashes(
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        options: UtpConnectionOptions,
        policy: &OutboundNetworkPolicy,
    ) -> Result<Self> {
        let utp_conn = match options.shared_socket {
            Some(socket) => {
                UtpPeerConnection::connect_with_shared_socket_hybrid(
                    socket,
                    addr,
                    info_hash_v1,
                    info_hash_v2,
                    &options.local_peer_id,
                    options.timeout,
                    options.dht_enabled,
                )
                .await?
            }
            None => {
                UtpPeerConnection::connect_with_policy(
                    addr,
                    info_hash_v1,
                    info_hash_v2,
                    &options.local_peer_id,
                    options.timeout,
                    options.listen_port,
                    options.dht_enabled,
                    policy,
                )
                .await?
            }
        };
        let now = Instant::now();

        Ok(Self {
            inner: InnerConnection::Utp(utp_conn),
            ip_addr: addr.ip().to_string(),
            port: addr.port(),
            peer_id: None,
            incoming: false,
            source: crate::request::request_group::BtPeerSource::Unknown,
            local_peer: false,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Utp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
            pex_enabled: true,
            upload_state: None,
            upload_progress: None,
        })
    }
}
