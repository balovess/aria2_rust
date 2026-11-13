//! Engine adapter from the process-owned uTP actor to BitTorrent peer sessions.

use crate::error::{Aria2Error, RecoverableError, Result};
use aria2_protocol::bittorrent::peer::session::{PeerTransport, PeerWireSession};

use super::super::utp_transport::{UtpConnectionHandle, UtpTransportHandle};

pub struct UtpPeerConnection {
    session: PeerWireSession<UtpConnectionHandle>,
    remote_endpoint: std::net::SocketAddr,
}

impl UtpPeerConnection {
    pub(crate) async fn connect_with_transport_hybrid(
        transport: &UtpTransportHandle,
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
    ) -> Result<Self> {
        let mut connection = transport.connect(addr).await.map_err(network_error)?;
        tokio::time::timeout(timeout, connection.wait_established(timeout))
            .await
            .map_err(|_| network_error("uTP connection setup timed out"))?
            .map_err(network_error)?;

        let session = PeerWireSession::connect_outgoing(
            connection,
            info_hash_v1,
            info_hash_v2,
            local_peer_id,
            dht_enabled,
            timeout,
        )
        .await
        .map_err(network_error)?;
        Ok(Self {
            session,
            remote_endpoint: addr,
        })
    }

    pub(crate) async fn receive_incoming_handshake(
        connection: UtpConnectionHandle,
        endpoint: std::net::SocketAddr,
        timeout: std::time::Duration,
    ) -> std::result::Result<Self, String> {
        let session = PeerWireSession::receive_incoming(connection, timeout).await?;
        Ok(Self {
            session,
            remote_endpoint: endpoint,
        })
    }

    pub(crate) async fn complete_incoming_handshake(
        &mut self,
        local_peer_id: &[u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        dht_enabled: bool,
    ) -> std::result::Result<(), String> {
        self.session
            .complete_incoming_handshake(local_peer_id, info_hash_v2, dht_enabled)
            .await
    }

    pub(crate) fn session_remote_handshake(
        &self,
    ) -> Option<&aria2_protocol::bittorrent::message::handshake::Handshake> {
        self.session.remote_handshake()
    }

    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        self.session
            .remote_handshake()
            .map(|handshake| handshake.peer_id)
    }

    pub fn remote_supports_dht(&self) -> bool {
        self.session
            .remote_handshake()
            .is_some_and(|handshake| handshake.supports_dht())
    }

    pub fn remote_supports_fast_extension(&self) -> bool {
        self.session
            .remote_handshake()
            .is_some_and(|handshake| handshake.supports_fast_extension())
    }

    pub fn remote_supports_extended_messaging(&self) -> bool {
        self.session
            .remote_handshake()
            .is_some_and(|handshake| handshake.supports_extended_messaging())
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        Some(self.remote_endpoint)
    }

    pub fn is_connected(&self) -> bool {
        self.session.is_connected()
    }

    /// Send one serialized BitTorrent message through the uTP actor.
    pub async fn send_message(&mut self, message: &[u8]) -> Result<()> {
        self.session
            .send_message(message)
            .await
            .map_err(network_error)
    }

    pub async fn send_bt_message(
        &mut self,
        message: &aria2_protocol::bittorrent::message::types::BtMessage,
    ) -> Result<()> {
        self.session
            .send_bt_message(message)
            .await
            .map_err(network_error)
    }

    /// Receive one framed BitTorrent message from the uTP actor.
    pub async fn recv_message(&mut self) -> Result<Option<Vec<u8>>> {
        self.session.receive_message().await.map_err(network_error)
    }

    pub async fn recv_bt_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        self.session
            .receive_bt_message()
            .await
            .map_err(map_peer_message_receive_error)
    }

    pub async fn close(&mut self) -> Result<()> {
        self.session.close().await.map_err(network_error)
    }
}

#[async_trait::async_trait]
impl PeerTransport for UtpConnectionHandle {
    async fn send(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        UtpConnectionHandle::send(self, bytes).await
    }

    async fn receive(&mut self) -> std::result::Result<Option<Vec<u8>>, String> {
        UtpConnectionHandle::recv(self).await
    }

    async fn close(&mut self) -> std::result::Result<(), String> {
        UtpConnectionHandle::close(self).await
    }
}

fn network_error(error: impl ToString) -> Aria2Error {
    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
        message: error.to_string(),
    })
}

fn map_peer_message_receive_error(
    error: aria2_protocol::bittorrent::peer::session::PeerMessageReceiveError,
) -> Aria2Error {
    match error {
        aria2_protocol::bittorrent::peer::session::PeerMessageReceiveError::Transport(message) => {
            network_error(message)
        }
        aria2_protocol::bittorrent::peer::session::PeerMessageReceiveError::InvalidMessage(
            message,
        ) => network_error(format!("invalid BitTorrent peer message: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utp_peer_frame_is_a_recoverable_network_failure() {
        let error = map_peer_message_receive_error(
            aria2_protocol::bittorrent::peer::session::PeerMessageReceiveError::InvalidMessage(
                "unknown message id".to_string(),
            ),
        );

        assert!(matches!(
            error,
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message })
                if message.contains("invalid BitTorrent peer message")
        ));
    }
}
