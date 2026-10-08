//! Actor-backed uTP connection used by BitTorrent peer actors.

use bytes::BytesMut;

use crate::error::{Aria2Error, RecoverableError, Result};

use super::super::utp_transport::{UtpConnectionHandle, UtpTransportHandle};

pub struct UtpPeerConnection {
    connection: UtpConnectionHandle,
    info_hash: [u8; 20],
    info_hash_v2: Option<[u8; 32]>,
    handshake_complete: bool,
    remote_peer_id: Option<[u8; 20]>,
    remote_supports_dht: bool,
    remote_supports_fast_extension: bool,
    remote_supports_extended_messaging: bool,
    remote_endpoint: Option<std::net::SocketAddr>,
    recv_buffer: BytesMut,
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
        let connection = transport.connect(addr).await.map_err(network_error)?;
        let mut peer = Self {
            connection,
            info_hash: *info_hash_v1,
            info_hash_v2: info_hash_v2.copied(),
            handshake_complete: false,
            remote_peer_id: None,
            remote_supports_dht: false,
            remote_supports_fast_extension: false,
            remote_supports_extended_messaging: false,
            remote_endpoint: Some(addr),
            recv_buffer: BytesMut::new(),
        };

        tokio::time::timeout(timeout, peer.connection.wait_established(timeout))
            .await
            .map_err(|_| network_error("uTP connection setup timed out"))?
            .map_err(network_error)?;
        tokio::time::timeout(timeout, peer.perform_handshake(local_peer_id, dht_enabled))
            .await
            .map_err(|_| network_error("uTP BitTorrent handshake timed out"))??;
        Ok(peer)
    }

    pub(crate) async fn receive_incoming_handshake(
        connection: UtpConnectionHandle,
        endpoint: std::net::SocketAddr,
        timeout: std::time::Duration,
    ) -> std::result::Result<
        (
            Self,
            aria2_protocol::bittorrent::message::handshake::Handshake,
        ),
        String,
    > {
        use aria2_protocol::bittorrent::message::handshake::Handshake;

        let mut peer = Self {
            connection,
            info_hash: [0; 20],
            info_hash_v2: None,
            handshake_complete: false,
            remote_peer_id: None,
            remote_supports_dht: false,
            remote_supports_fast_extension: false,
            remote_supports_extended_messaging: false,
            remote_endpoint: Some(endpoint),
            recv_buffer: BytesMut::new(),
        };
        tokio::time::timeout(timeout, peer.read_bytes(68))
            .await
            .map_err(|_| "uTP BitTorrent handshake timed out".to_string())??;
        let handshake = Handshake::parse(&peer.recv_buffer.split_to(68))?;
        peer.info_hash = handshake.info_hash;
        peer.remote_peer_id = Some(handshake.peer_id);
        peer.remote_supports_dht = handshake.supports_dht();
        peer.remote_supports_fast_extension = handshake.supports_fast_extension();
        peer.remote_supports_extended_messaging = handshake.supports_extended_messaging();
        Ok((peer, handshake))
    }

    pub(crate) async fn complete_incoming_handshake(
        &mut self,
        handshake: &aria2_protocol::bittorrent::message::handshake::Handshake,
        local_peer_id: &[u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        dht_enabled: bool,
    ) -> std::result::Result<(), String> {
        use aria2_protocol::bittorrent::message::handshake::Handshake;

        self.info_hash_v2 = info_hash_v2;
        let response_hash = info_hash_v2
            .filter(|_| handshake.supports_bep52())
            .map(|hash| hash[..20].try_into().expect("SHA-256 hash is 32 bytes"))
            .unwrap_or(handshake.info_hash);
        self.connection
            .send(
                &Handshake::new(&response_hash, local_peer_id)
                    .with_dht(dht_enabled)
                    .with_bep52(info_hash_v2.is_some())
                    .to_bytes(),
            )
            .await?;
        self.handshake_complete = true;
        Ok(())
    }

    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        self.remote_peer_id
    }

    pub fn remote_supports_dht(&self) -> bool {
        self.remote_supports_dht
    }

    pub fn remote_supports_fast_extension(&self) -> bool {
        self.remote_supports_fast_extension
    }

    pub fn remote_supports_extended_messaging(&self) -> bool {
        self.remote_supports_extended_messaging
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_endpoint
    }

    pub fn is_connected(&self) -> bool {
        self.handshake_complete
    }

    async fn perform_handshake(
        &mut self,
        local_peer_id: &[u8; 20],
        dht_enabled: bool,
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::handshake::Handshake;

        let handshake = Handshake::new(&self.info_hash, local_peer_id)
            .with_dht(dht_enabled)
            .with_bep52(self.info_hash_v2.is_some());
        self.connection
            .send(&handshake.to_bytes())
            .await
            .map_err(network_error)?;
        self.read_bytes(68).await.map_err(network_error)?;

        let response = Handshake::parse(&self.recv_buffer.split_to(68)).map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: error })
        })?;
        let accepted = response.info_hash == self.info_hash
            || self
                .info_hash_v2
                .is_some_and(|hash| response.info_hash == hash[..20] && response.supports_bep52());
        if !accepted {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "uTP peer BitTorrent info-hash mismatch".to_string(),
                },
            ));
        }

        self.remote_peer_id = Some(response.peer_id);
        self.remote_supports_dht = response.supports_dht();
        self.remote_supports_fast_extension = response.supports_fast_extension();
        self.remote_supports_extended_messaging = response.supports_extended_messaging();
        self.handshake_complete = true;
        Ok(())
    }

    async fn read_bytes(&mut self, length: usize) -> std::result::Result<(), String> {
        while self.recv_buffer.len() < length {
            let Some(bytes) = self.connection.recv().await? else {
                return Err("uTP connection closed while receiving BitTorrent data".to_string());
            };
            self.recv_buffer.extend_from_slice(&bytes);
        }
        Ok(())
    }

    /// Send a BitTorrent wire frame through the process-owned uTP actor.
    pub async fn send_message(&mut self, message: &[u8]) -> Result<()> {
        self.connection.send(message).await.map_err(network_error)
    }

    /// Receive one length-prefixed BitTorrent wire frame.
    pub async fn recv_message(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if self.recv_buffer.len() >= 4 {
                let message_len =
                    u32::from_be_bytes(self.recv_buffer[..4].try_into().unwrap()) as usize;
                let frame_len = message_len.checked_add(4).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(
                        "uTP BitTorrent message length overflows address space".to_string(),
                    ))
                })?;
                if self.recv_buffer.len() >= frame_len {
                    return Ok(Some(self.recv_buffer.split_to(frame_len).to_vec()));
                }
            }

            let Some(bytes) = self.connection.recv().await.map_err(network_error)? else {
                if self.recv_buffer.is_empty() {
                    return Ok(None);
                }
                return Err(network_error(
                    "uTP connection closed with an incomplete BitTorrent message",
                ));
            };
            self.recv_buffer.extend_from_slice(&bytes);
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.connection.close().await.map_err(network_error)
    }
}

fn network_error(error: impl ToString) -> Aria2Error {
    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
        message: error.to_string(),
    })
}
