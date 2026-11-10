//! BitTorrent peer handshake and message framing over an injected transport.

use bytes::{Bytes, BytesMut};
use std::time::Duration;

use crate::bittorrent::message::handshake::Handshake;

/// Byte transport used by a BitTorrent peer session.
///
/// Implementations own connection establishment and transport lifecycle.
/// This interface carries arbitrary byte chunks; the peer session owns the
/// BitTorrent handshake and length-prefixed message framing. `send` must
/// preserve byte order and report success only after the whole slice is sent;
/// `receive` returns non-empty chunks or `None` at end of stream.
#[async_trait::async_trait]
pub trait PeerTransport: Send {
    async fn send(&mut self, bytes: &[u8]) -> Result<(), String>;
    async fn receive(&mut self) -> Result<Option<Vec<u8>>, String>;
    async fn close(&mut self) -> Result<(), String>;
}

/// Incremental decoder for length-prefixed BitTorrent peer messages.
#[derive(Default)]
pub struct PeerFrameDecoder {
    buffer: BytesMut,
}

impl PeerFrameDecoder {
    /// Append bytes received from the transport.
    pub fn append(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Return the number of buffered bytes not yet consumed.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Return whether the decoder has no buffered bytes.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Remove and return a prefix when at least `length` bytes are buffered.
    pub fn take_prefix(&mut self, length: usize) -> Option<Bytes> {
        (self.buffer.len() >= length).then(|| self.buffer.split_to(length).freeze())
    }

    /// Return the next complete frame, or `None` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Bytes>, String> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }

        let message_len = u32::from_be_bytes(self.buffer[..4].try_into().unwrap()) as usize;
        if message_len > crate::bittorrent::message::MAX_BT_MESSAGE_LENGTH {
            return Err(format!(
                "BitTorrent message length {} exceeds maximum {}",
                message_len,
                crate::bittorrent::message::MAX_BT_MESSAGE_LENGTH
            ));
        }
        let frame_len = 4 + message_len;
        if self.buffer.len() < frame_len {
            return Ok(None);
        }
        Ok(Some(self.buffer.split_to(frame_len).freeze()))
    }
}

/// BitTorrent peer protocol state over one caller-provided byte transport.
pub struct PeerWireSession<T> {
    transport: T,
    frame_decoder: PeerFrameDecoder,
    remote_handshake: Option<Handshake>,
    handshake_complete: bool,
}

impl<T: PeerTransport> PeerWireSession<T> {
    /// Perform an outgoing BitTorrent handshake and retain the transport.
    pub async fn connect_outgoing(
        transport: T,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        dht_enabled: bool,
        timeout: Duration,
    ) -> Result<Self, String> {
        let mut session = Self::new(transport);
        tokio::time::timeout(timeout, async {
            let handshake = Handshake::new(info_hash_v1, local_peer_id)
                .with_dht(dht_enabled)
                .with_bep52(info_hash_v2.is_some());
            session.transport.send(&handshake.to_bytes()).await?;
            session.read_bytes(68).await?;

            let handshake_bytes = session
                .frame_decoder
                .take_prefix(68)
                .ok_or_else(|| "BitTorrent handshake buffer is incomplete".to_string())?;
            let remote = Handshake::parse(&handshake_bytes)?;
            if !remote.matches_info_hash(info_hash_v1, info_hash_v2) {
                return Err("BitTorrent peer info-hash mismatch".to_string());
            }
            session.remote_handshake = Some(remote);
            session.handshake_complete = true;
            Ok::<(), String>(())
        })
        .await
        .map_err(|_| "BitTorrent peer handshake timed out".to_string())??;
        Ok(session)
    }

    /// Receive an incoming BitTorrent handshake before the torrent route is
    /// known. Call [`complete_incoming_handshake`](Self::complete_incoming_handshake)
    /// after selecting the route.
    pub async fn receive_incoming(transport: T, timeout: Duration) -> Result<Self, String> {
        let mut session = Self::new(transport);
        tokio::time::timeout(timeout, async {
            session.read_bytes(68).await?;
            let handshake_bytes = session
                .frame_decoder
                .take_prefix(68)
                .ok_or_else(|| "BitTorrent handshake buffer is incomplete".to_string())?;
            session.remote_handshake = Some(Handshake::parse(&handshake_bytes)?);
            Ok::<(), String>(())
        })
        .await
        .map_err(|_| "BitTorrent peer handshake timed out".to_string())??;
        Ok(session)
    }

    /// Send this session's response to an incoming handshake after route
    /// selection.
    pub async fn complete_incoming_handshake(
        &mut self,
        local_peer_id: &[u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        dht_enabled: bool,
    ) -> Result<(), String> {
        let remote = self
            .remote_handshake
            .as_ref()
            .ok_or_else(|| "Incoming peer handshake has not been received".to_string())?;
        let response_hash = info_hash_v2
            .filter(|_| remote.supports_bep52())
            .map(|hash| hash[..20].try_into().expect("SHA-256 hash is 32 bytes"))
            .unwrap_or(remote.info_hash);
        let response = Handshake::new(&response_hash, local_peer_id)
            .with_dht(dht_enabled)
            .with_bep52(info_hash_v2.is_some());
        self.transport.send(&response.to_bytes()).await?;
        self.handshake_complete = true;
        Ok(())
    }

    /// Return the peer's parsed handshake, if one has been received.
    pub fn remote_handshake(&self) -> Option<&Handshake> {
        self.remote_handshake.as_ref()
    }

    pub fn is_connected(&self) -> bool {
        self.handshake_complete
    }

    /// Send one already-serialized, length-prefixed BitTorrent message.
    pub async fn send_message(&mut self, frame: &[u8]) -> Result<(), String> {
        self.transport.send(frame).await
    }

    /// Receive one complete length-prefixed BitTorrent message.
    pub async fn receive_message(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            if let Some(frame) = self.frame_decoder.next_frame()? {
                return Ok(Some(frame.to_vec()));
            }

            let Some(bytes) = self.transport.receive().await? else {
                return if self.frame_decoder.is_empty() {
                    Ok(None)
                } else {
                    Err("Transport closed with an incomplete BitTorrent message".to_string())
                };
            };
            self.frame_decoder.append(&bytes);
        }
    }

    pub async fn close(&mut self) -> Result<(), String> {
        self.transport.close().await
    }

    fn new(transport: T) -> Self {
        Self {
            transport,
            frame_decoder: PeerFrameDecoder::default(),
            remote_handshake: None,
            handshake_complete: false,
        }
    }

    async fn read_bytes(&mut self, length: usize) -> Result<(), String> {
        while self.frame_decoder.buffered_len() < length {
            let Some(bytes) = self.transport.receive().await? else {
                return Err("Transport closed during BitTorrent handshake".to_string());
            };
            self.frame_decoder.append(&bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct ChannelTransport {
        outgoing: mpsc::Sender<Vec<u8>>,
        incoming: mpsc::Receiver<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl PeerTransport for ChannelTransport {
        async fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.outgoing
                .send(bytes.to_vec())
                .await
                .map_err(|_| "test transport closed".to_string())
        }

        async fn receive(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(self.incoming.recv().await)
        }

        async fn close(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    fn transport_pair() -> (ChannelTransport, ChannelTransport) {
        let (first_tx, first_rx) = mpsc::channel(4);
        let (second_tx, second_rx) = mpsc::channel(4);
        (
            ChannelTransport {
                outgoing: first_tx,
                incoming: second_rx,
            },
            ChannelTransport {
                outgoing: second_tx,
                incoming: first_rx,
            },
        )
    }

    #[tokio::test]
    async fn outgoing_handshake_preserves_bytes_received_after_handshake() {
        let info_hash = [0x42; 20];
        let local_peer_id = [0x11; 20];
        let remote_peer_id = [0x22; 20];
        let (transport, mut server) = transport_pair();
        let server_task = tokio::spawn(async move {
            let request = server.receive().await.unwrap().unwrap();
            let request = Handshake::parse(&request).unwrap();
            assert_eq!(request.info_hash, info_hash);

            let response = Handshake::new(&info_hash, &remote_peer_id).to_bytes();
            let mut response_and_keepalive = response.to_vec();
            response_and_keepalive.extend_from_slice(&[0, 0, 0, 0]);
            server.send(&response_and_keepalive).await.unwrap();
        });

        let mut session = PeerWireSession::connect_outgoing(
            transport,
            &info_hash,
            None,
            &local_peer_id,
            false,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(session.is_connected());
        assert_eq!(session.remote_handshake().unwrap().peer_id, remote_peer_id);
        assert_eq!(
            session.receive_message().await.unwrap(),
            Some(vec![0, 0, 0, 0])
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn incoming_handshake_response_uses_v2_hash_for_hybrid_peer() {
        let info_hash_v1 = [0x31; 20];
        let info_hash_v2 = [0x53; 32];
        let remote_peer_id = [0x77; 20];
        let local_peer_id = [0x66; 20];
        let mut remote_handshake = Handshake::new(&info_hash_v1, &remote_peer_id)
            .with_bep52(true)
            .to_bytes();
        remote_handshake[28..48].copy_from_slice(&info_hash_v2[..20]);
        let (transport, mut remote) = transport_pair();
        remote.send(&remote_handshake).await.unwrap();

        let mut session = PeerWireSession::receive_incoming(transport, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            session.remote_handshake().unwrap().info_hash,
            info_hash_v2[..20]
        );
        session
            .complete_incoming_handshake(&local_peer_id, Some(info_hash_v2), false)
            .await
            .unwrap();
        let response = Handshake::parse(&remote.receive().await.unwrap().unwrap()).unwrap();
        assert_eq!(response.info_hash, info_hash_v2[..20]);
        assert!(session.is_connected());
    }

    #[tokio::test]
    async fn rejects_oversized_peer_message_before_waiting_for_body() {
        let info_hash = [0x12; 20];
        let remote_peer_id = [0x34; 20];
        let (transport, mut remote) = transport_pair();
        let local_peer_id = [0x56; 20];
        let connect_task = tokio::spawn(async move {
            PeerWireSession::connect_outgoing(
                transport,
                &info_hash,
                None,
                &local_peer_id,
                false,
                Duration::from_secs(1),
            )
            .await
            .unwrap()
        });
        let request = Handshake::parse(&remote.receive().await.unwrap().unwrap()).unwrap();
        remote
            .send(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();
        let mut session = connect_task.await.unwrap();
        remote
            .send(&((crate::bittorrent::message::MAX_BT_MESSAGE_LENGTH as u32 + 1).to_be_bytes()))
            .await
            .unwrap();
        assert!(
            session
                .receive_message()
                .await
                .unwrap_err()
                .contains("exceeds maximum")
        );
        assert_eq!(request.info_hash, info_hash);
    }
}
