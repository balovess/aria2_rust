//! Connection constructors for [`BtPeerConn`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use crate::engine::peer_stats::PeerStats;
use crate::error::{Aria2Error, FatalError, Result};

use super::super::types::{ConnectionType, SendBuffer};
use super::super::utp_connection::UtpPeerConnection;
use super::{BtPeerConn, InnerConnection};

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Connection constructors
    // -----------------------------------------------------------------------

    /// Connect via MSE (Message Stream Encryption) over TCP.
    pub async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Encrypted(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
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
                    stats: PeerStats::new([0u8; 20], std::net::SocketAddr::new(
                        addr.ip.parse().map_err(|_| {
                                Aria2Error::Fatal(FatalError::Config(format!(
                                    "Invalid peer IP address: {}",
                                    addr.ip
                                )))
                            })?,
                            addr.port,
                    )),
                    pending_pex_peers: Vec::new(),
                })
            }
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via plain TCP.
    pub async fn connect_plain(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash)
            .await
        {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Plain(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
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
        let peer_id = conn.remote_peer_id;
        Self {
            inner: InnerConnection::Plain(conn),
            ip_addr: endpoint.ip().to_string(),
            port: endpoint.port(),
            peer_id,
            incoming: true,
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
            stats: PeerStats::new(peer_id.unwrap_or([0u8; 20]), endpoint),
            pending_pex_peers: Vec::new(),
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
            stats: PeerStats::new(peer_id.unwrap_or([0u8; 20]), endpoint),
            pending_pex_peers: Vec::new(),
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
                stream, [0u8; 20],
            );

        Self {
            inner: InnerConnection::Plain(peer_conn),
            ip_addr: "127.0.0.1".to_string(),
            port: 0,
            peer_id: Some(*info_hash),
            incoming: false,
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
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
        }
    }

    /// Connect via uTP (Micro Transport Protocol).
    ///
    /// uTP is a UDP-based transport protocol that provides:
    /// - Reliable, ordered delivery
    /// - LEDBAT congestion control (low priority, background traffic)
    /// - Better performance on congested networks
    /// - NAT traversal benefits
    pub async fn connect_utp(addr: std::net::SocketAddr, info_hash: &[u8; 20]) -> Result<Self> {
        let utp_conn = UtpPeerConnection::connect(addr, info_hash).await?;
        let now = Instant::now();

        Ok(Self {
            inner: InnerConnection::Utp(utp_conn),
            ip_addr: addr.ip().to_string(),
            port: addr.port(),
            peer_id: None,
            incoming: false,
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
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
        })
    }

    /// Create a uTP connection from an existing socket.
    ///
    /// Used when accepting incoming uTP connections.
    pub fn from_utp_socket(
        socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
        conn_id: u16,
        info_hash: &[u8; 20],
    ) -> Self {
        let utp_conn = UtpPeerConnection::new(socket, conn_id, *info_hash);
        let now = Instant::now();

        Self {
            inner: InnerConnection::Utp(utp_conn),
            ip_addr: String::new(),
            port: 0,
            peer_id: None,
            incoming: true,
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
            stats: PeerStats::new(
                [0u8; 20],
                std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
            ),
            pending_pex_peers: Vec::new(),
        }
    }
}
