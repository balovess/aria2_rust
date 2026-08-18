use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info};

use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::extension::mse_handshake::{MSE_PUBLIC_KEY_LENGTH, MseHandshake};
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use crate::bittorrent::peer::connection::{PeerAddr, PeerConnection};
use crate::bittorrent::peer::state::PeerState;

pub struct EncryptedConnection {
    inner: PeerConnection,
    crypto: MseCryptoState,
    mse_negotiated: bool,
    read_ahead: Vec<u8>,
    // Store decrypted frame bytes so a cancelled read can resume safely.
    read_buffer: BytesMut,
}

impl EncryptedConnection {
    /// Wrap a TCP stream after the receiver-side MSE and BitTorrent
    /// handshakes have completed.
    pub fn from_incoming_parts(
        stream: tokio::net::TcpStream,
        crypto: MseCryptoState,
        peer_id: [u8; 20],
    ) -> Self {
        Self {
            inner: PeerConnection::from_stream_with_peer(stream, peer_id),
            crypto,
            mse_negotiated: true,
            read_ahead: Vec::new(),
            read_buffer: BytesMut::new(),
        }
    }

    pub async fn connect_with_mse(
        addr: &PeerAddr,
        info_hash: &[u8; 20],
        force_encryption: bool,
        prefer_encryption: bool,
    ) -> Result<Self, String> {
        let local_peer_id = crate::bittorrent::peer::id::generate_peer_id();
        Self::connect_with_mse_with_options(
            addr,
            info_hash,
            force_encryption,
            prefer_encryption,
            &local_peer_id,
            std::time::Duration::from_secs(15),
        )
        .await
    }

    pub async fn connect_with_mse_with_options(
        addr: &PeerAddr,
        info_hash: &[u8; 20],
        force_encryption: bool,
        prefer_encryption: bool,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let socket_addr = addr.to_socket_addr();
        debug!("MSE connecting to peer: {}", socket_addr);

        let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&socket_addr))
        .await
        .map_err(|_| format!("Connection to peer timed out: {}", socket_addr))?
        .map_err(|e| format!("Failed to connect to peer: {}", e))?;

        // aria2's MSE path starts with DH. The 68-byte BitTorrent handshake
        // is exchanged only after MSE has selected the stream cipher.
        Self::complete_mse_handshake(
            stream,
            info_hash,
            force_encryption,
            prefer_encryption,
            local_peer_id,
            timeout,
        )
        .await
    }

    async fn complete_mse_handshake(
        mut stream: tokio::net::TcpStream,
        info_hash: &[u8; 20],
        force_encryption: bool,
        prefer_encryption: bool,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let mut initiator = MseHandshake::new_initiator(*info_hash);
        initiator.set_crypto_preferences(force_encryption, prefer_encryption);

        // Step 1: Exchange DH public keys
        let step1_i = initiator.build_step1();
        stream
            .write_all(&step1_i)
            .await
            .map_err(|e| format!("MSE Step1 send failed: {}", e))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("MSE Step1 flush failed: {}", e))?;

        // PadB has no explicit length. The later VC marker synchronizes the
        // response, just as MSEHandshake::findInitiatorVCMarker does upstream.
        let mut step1_r_buf = vec![0u8; MSE_PUBLIC_KEY_LENGTH];
        Self::read_exact_with_timeout(&mut stream, &mut step1_r_buf, "MSE public key").await?;

        initiator.receive_step1(&step1_r_buf)?;

        // Step 3 (initiator): Send req1 + req2^req3 + encrypted payload
        let step3_i = initiator.build_initiator_step2()?;
        stream
            .write_all(&step3_i)
            .await
            .map_err(|e| format!("MSE Step3 send failed: {}", e))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("MSE Step3 flush failed: {}", e))?;

        let mut step4_r_buf = Vec::with_capacity(1_152);
        let mut read_ahead = Vec::new();
        let response_len = loop {
            let mut chunk = [0u8; 64];
            let read_len =
                tokio::time::timeout(timeout, stream.read(&mut chunk))
                    .await
                    .map_err(|_| "MSE response read timeout".to_string())?
                    .map_err(|error| format!("MSE response read failed: {error}"))?;
            if read_len == 0 {
                return Err("MSE response: peer closed connection".to_string());
            }
            step4_r_buf.extend_from_slice(&chunk[..read_len]);
            if let Some(length) = initiator.initiator_step2_required_len(&step4_r_buf)?
                && step4_r_buf.len() >= length
            {
                if step4_r_buf.len() > length {
                    read_ahead.extend_from_slice(&step4_r_buf.split_off(length));
                }
                break length;
            }
            if step4_r_buf.len() >= 1_152 {
                return Err("MSE response exceeded handshake buffer limit".to_string());
            }
        };
        step4_r_buf.truncate(response_len);
        initiator.receive_receiver_step2(&step4_r_buf)?;
        let mut crypto = initiator.finalize()?;

        info!(
            "MSE handshake complete: encrypted={}",
            crypto.is_encrypted()
        );

        let mut local_handshake = Handshake::new(info_hash, &local_peer_id)
            .with_dht(true)
            .to_bytes();
        crypto.encrypt(&mut local_handshake);
        stream
            .write_all(&local_handshake)
            .await
            .map_err(|error| format!("Failed to send encrypted handshake: {error}"))?;

        let mut remote_handshake = [0u8; 68];
        Self::read_exact_with_pending(
            &mut stream,
            &mut read_ahead,
            &mut remote_handshake,
            "MSE handshake",
            timeout,
        )
        .await?;
        crypto.decrypt(&mut remote_handshake);
        let remote_hs = Handshake::parse(&remote_handshake).map_err(|error| {
            format!(
                "{error}; decrypted handshake prefix={:02x?}",
                &remote_handshake[..8]
            )
        })?;
        if remote_hs.info_hash != *info_hash {
            return Err("info_hash mismatch".to_string());
        }
        let conn = PeerConnection::from_stream_with_peer(stream, remote_hs.peer_id);

        Ok(Self {
            inner: conn,
            crypto,
            mse_negotiated: true,
            read_ahead,
            read_buffer: BytesMut::new(),
        })
    }

    async fn read_exact_with_timeout(
        stream: &mut tokio::net::TcpStream,
        buffer: &mut [u8],
        label: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        tokio::time::timeout(timeout, stream.read_exact(buffer))
        .await
        .map_err(|_| format!("{label} read timeout"))?
        .map(|_| ())
        .map_err(|error| format!("{label} read failed: {error}"))
    }

    async fn read_exact_with_pending(
        stream: &mut tokio::net::TcpStream,
        pending: &mut Vec<u8>,
        buffer: &mut [u8],
        label: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        let copied = pending.len().min(buffer.len());
        buffer[..copied].copy_from_slice(&pending[..copied]);
        pending.drain(..copied);
        if copied < buffer.len() {
            Self::read_exact_with_timeout(stream, &mut buffer[copied..], label, timeout).await?;
        }
        Ok(())
    }

    pub fn is_encrypted(&self) -> bool {
        self.crypto.is_encrypted()
    }

    pub fn is_mse_negotiated(&self) -> bool {
        self.mse_negotiated
    }

    pub async fn send_message(&mut self, message: &BtMessage) -> Result<(), String> {
        use crate::bittorrent::message::serializer::serialize;
        let data = serialize(message);
        self.send_encrypted(&data).await
    }

    /// Send an already-framed BitTorrent message without serializing it again.
    ///
    /// Encryption still requires a per-connection copy for the cipher, but the
    /// common wire-frame construction is shared across a HAVE broadcast.
    pub async fn send_serialized(&mut self, data: &[u8]) -> Result<(), String> {
        self.send_encrypted(data).await
    }

    pub async fn read_message(&mut self) -> Result<Option<BtMessage>, String> {
        if !self.read_ahead.is_empty() {
            let mut pending = std::mem::take(&mut self.read_ahead);
            self.crypto.decrypt(&mut pending);
            self.read_buffer.extend_from_slice(&pending);
        }

        loop {
            if self.read_buffer.len() >= 4 {
                let msg_len =
                    u32::from_be_bytes(self.read_buffer[..4].try_into().unwrap()) as usize;
                let frame_len = 4 + msg_len;
                if self.read_buffer.len() >= frame_len {
                    let frame = self.read_buffer.split_to(frame_len).freeze();
                    if msg_len == 0 {
                        return Ok(Some(BtMessage::KeepAlive));
                    }
                    return crate::bittorrent::message::factory::parse_message_bytes(frame);
                }
            }

            let mut encrypted_chunk = [0u8; 16 * 1024];
            let bytes_read = self.inner.stream_read(&mut encrypted_chunk).await?;
            if bytes_read == 0 {
                return if self.read_buffer.is_empty() {
                    Ok(None)
                } else {
                    Err("Failed to read encrypted message: unexpected eof".to_string())
                };
            }
            self.crypto.decrypt(&mut encrypted_chunk[..bytes_read]);
            self.read_buffer
                .extend_from_slice(&encrypted_chunk[..bytes_read]);
        }
    }

    async fn send_encrypted(&mut self, data: &[u8]) -> Result<(), String> {
        let mut buf = data.to_vec();
        self.crypto.encrypt(&mut buf);

        self.inner.stream_write(&buf).await?;
        self.inner.stream_flush().await?;

        debug!("Sent encrypted message: {} bytes", buf.len());
        Ok(())
    }

    pub async fn send_choke(&mut self) -> Result<(), String> {
        self.inner.state.am_choking = true;
        self.send_message(&BtMessage::Choke).await
    }

    pub async fn send_unchoke(&mut self) -> Result<(), String> {
        self.inner.state.am_choking = false;
        self.send_message(&BtMessage::Unchoke).await
    }

    pub async fn send_interested(&mut self) -> Result<(), String> {
        self.inner.state.am_interested = true;
        self.send_message(&BtMessage::Interested).await
    }

    pub async fn send_not_interested(&mut self) -> Result<(), String> {
        self.inner.state.am_interested = false;
        self.send_message(&BtMessage::NotInterested).await
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<(), String> {
        self.send_message(&BtMessage::Have { piece_index }).await
    }

    pub async fn send_request(&mut self, req: PieceBlockRequest) -> Result<(), String> {
        self.inner.state.add_request(req.clone());
        self.send_message(&BtMessage::Request { request: req })
            .await
    }

    pub async fn send_cancel(&mut self, req: &PieceBlockRequest) -> Result<(), String> {
        self.inner.state.remove_request(req);
        self.send_message(&BtMessage::Cancel {
            request: req.clone(),
        })
        .await
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<(), String> {
        self.inner.remote_bitfield = bitfield.clone();
        self.send_message(&BtMessage::Bitfield { data: bitfield })
            .await
    }

    pub fn state(&self) -> &PeerState {
        &self.inner.state
    }

    pub fn remote_peer_id(&self) -> Option<&[u8; 20]> {
        self.inner.remote_peer_id.as_ref()
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.inner.remote_addr()
    }

    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_encrypted_flag() {
        let secret = [0x42u8; 96];
        let info_hash = [0xABu8; 20];
        let keys =
            crate::bittorrent::extension::mse_crypto::MseDerivedKeys::derive(&secret, &info_hash);
        let enc = MseCryptoState::new_encrypted(&keys, true);
        assert!(enc.is_encrypted());

        let plain = MseCryptoState::new_plain();
        assert!(!plain.is_encrypted());
    }

    #[test]
    fn test_should_negotiate_all_combos() {
        // MSE reserved bit is at reserved[7] bit 0
        let reserved_zero = [0u8; 8];
        let mut reserved_mse = [0u8; 8];
        reserved_mse[7] = 0x01;
        let mut reserved_ff = [0u8; 8];
        reserved_ff[7] = 0xFF;

        assert!(!MseHandshake::should_negotiate(true, &reserved_zero));
        assert!(MseHandshake::should_negotiate(true, &reserved_mse));
        assert!(MseHandshake::should_negotiate(true, &reserved_ff));
        assert!(!MseHandshake::should_negotiate(false, &reserved_mse));
        assert!(!MseHandshake::should_negotiate(true, &[]));
    }

    #[tokio::test]
    async fn test_connect_unreachable_returns_err() {
        let result = EncryptedConnection::connect_with_mse(
            &PeerAddr::new("127.0.0.1", 1),
            &[0xAB; 20],
            false,
            false,
        )
        .await;
        assert!(result.is_err(), "unreachable address should fail");
    }

    #[tokio::test]
    async fn test_read_message_preserves_partial_frame_after_cancellation() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut connection = EncryptedConnection::from_incoming_parts(
            server,
            MseCryptoState::new_plain(),
            [0u8; 20],
        );
        let frame = crate::bittorrent::message::serializer::serialize(&BtMessage::Choke);

        client.write_all(&frame[..2]).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(25),
                connection.read_message()
            )
            .await
            .is_err()
        );

        client.write_all(&frame[2..]).await.unwrap();
        assert_eq!(
            connection.read_message().await.unwrap(),
            Some(BtMessage::Choke)
        );
    }
}
