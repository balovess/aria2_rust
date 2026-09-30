//! Connection constructors for [`BtPeerConn`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::engine::bittorrent::peer::stats::PeerStats;
use crate::error::{Aria2Error, FatalError, Result};
use crate::network::OutboundNetworkPolicy;

use super::super::types::{ConnectionType, SendBuffer};
use super::super::utp_connection::UtpPeerConnection;
use super::{BtPeerConn, InnerConnection, KEEPALIVE_INTERVAL_SECS, PEER_TIMEOUT_SECS};

use aria2_protocol::bittorrent::peer::mse::MseConnectionOptions;

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

    fn from_outgoing_transport(
        inner: InnerConnection,
        ip_addr: String,
        endpoint: std::net::SocketAddr,
        connection_type: ConnectionType,
    ) -> Self {
        let now = Instant::now();
        Self {
            actor_id: super::PeerActorId::allocate(),
            inner,
            ip_addr,
            port: endpoint.port(),
            remote_listen_port: None,
            peer_id: None,
            remote_client: Arc::new(std::sync::RwLock::new(None)),
            incoming: false,
            source: crate::request::request_group::BtPeerSource::Unknown,
            local_peer: false,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type,
            peer_allowed_fast: HashSet::new(),
            am_allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            keep_alive_interval: std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
            peer_timeout: std::time::Duration::from_secs(PEER_TIMEOUT_SECS),
            stats: PeerStats::new([0u8; 20], endpoint),
            pending_pex_peers: Vec::new(),
            pex_enabled: true,
            upload_state: None,
            upload_progress: None,
            actor_startup: None,
        }
    }

    fn from_incoming_transport(
        inner: InnerConnection,
        endpoint: std::net::SocketAddr,
        peer_id: Option<[u8; 20]>,
    ) -> Self {
        let now = Instant::now();
        Self {
            actor_id: super::PeerActorId::allocate(),
            inner,
            ip_addr: endpoint.ip().to_string(),
            port: endpoint.port(),
            remote_listen_port: None,
            peer_id,
            remote_client: Arc::new(std::sync::RwLock::new(None)),
            incoming: true,
            source: crate::request::request_group::BtPeerSource::Incoming,
            local_peer: endpoint.ip().is_loopback()
                || matches!(endpoint.ip(), std::net::IpAddr::V4(address) if address.is_private()),
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            peer_allowed_fast: HashSet::new(),
            am_allowed_fast: HashSet::new(),
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
            actor_startup: None,
        }
    }

    /// Connect through the process-wide outbound network policy.
    pub async fn connect_mse_with_policy(
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
        let connection = aria2_protocol::bittorrent::peer::mse::connect_with_stream(
            stream,
            info_hash_v1,
            info_hash_v2,
            options,
        )
        .await;
        match connection {
            Ok(conn) => Ok(Self::from_outgoing_transport(
                InnerConnection::Tcp(conn),
                addr.ip.clone(),
                socket_addr,
                ConnectionType::Tcp,
            )),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via plain TCP using the selected outbound network policy.
    pub async fn connect_plain_with_policy(
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
            Ok(conn) => Ok(Self::from_outgoing_transport(
                InnerConnection::Tcp(conn),
                addr.ip.clone(),
                socket_addr,
                ConnectionType::Tcp,
            )),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Wrap an already handshaken incoming TCP peer.
    pub(crate) fn from_incoming_tcp(
        conn: aria2_protocol::bittorrent::peer::connection::PeerConnection,
        endpoint: std::net::SocketAddr,
    ) -> Self {
        let peer_id = conn.remote_peer_id().copied();
        Self::from_incoming_transport(InnerConnection::Tcp(conn), endpoint, peer_id)
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
            actor_id: super::PeerActorId::allocate(),
            inner: InnerConnection::Tcp(peer_conn),
            ip_addr: "127.0.0.1".to_string(),
            port: 0,
            remote_listen_port: None,
            peer_id: Some(*info_hash),
            remote_client: Arc::new(std::sync::RwLock::new(None)),
            incoming: false,
            source: crate::request::request_group::BtPeerSource::Unknown,
            local_peer: true,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            peer_allowed_fast: HashSet::new(),
            am_allowed_fast: HashSet::new(),
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
            actor_startup: None,
        }
    }

    /// Connect via uTP using the selected outbound network policy.
    pub async fn connect_utp_with_policy(
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
        Ok(Self::from_outgoing_transport(
            InnerConnection::Utp(utp_conn),
            addr.ip().to_string(),
            addr,
            ConnectionType::Utp,
        ))
    }
}
