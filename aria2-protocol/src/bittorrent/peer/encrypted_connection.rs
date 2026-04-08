use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info};

use crate::bittorrent::peer::connection::{PeerConnection, PeerAddr};
use crate::bittorrent::peer::state::PeerState;
use crate::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::extension::mse_handshake::MseHandshake;
use crate::bittorrent::extension::mse_crypto::{MseCryptoMethod, MseCryptoState};

pub struct EncryptedConnection {
    inner: PeerConnection,
    crypto: MseCryptoState,
    mse_negotiated: bool,
}

impl EncryptedConnection {
    pub async fn connect_with_mse(
        addr: &PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self, String> {
        let socket_addr = addr.to_socket_addr();
        debug!("MSE连接peer: {}", socket_addr);

        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::net::TcpStream::connect(&socket_addr),
        ).await
        .map_err(|_| format!("连接peer超时: {}", socket_addr))?
        .map_err(|e| format!("连接peer失败: {}", e))?;

        let my_peer_id = crate::bittorrent::peer::id::generate_peer_id();
        let handshake = Handshake::new(info_hash, &my_peer_id).with_extensions(true);
        let handshake_bytes = handshake.to_bytes();

        stream.write_all(&handshake_bytes).await
            .map_err(|e| format!("发送握手失败: {}", e))?;

        let mut response = [0u8; 68];
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read_exact(&mut response)
        ).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("读取握手响应失败: {}", e)),
            Err(_) => return Err("读取握手响应超时".to_string()),
        }

        let remote_hs = Handshake::parse(&response)?;
        if remote_hs.info_hash != *info_hash {
            return Err("info_hash不匹配".to_string());
        }

        let local_supports_mse = true;

        if MseHandshake::should_negotiate(local_supports_mse, &remote_hs.reserved) {
            Self::complete_mse_handshake(stream, info_hash, &remote_hs, require_encryption)
                .await
        } else if require_encryption {
            Err(format!("Peer {} 不支持加密，但要求强制加密", socket_addr))
        } else {
            Ok(Self::from_plain_connection(stream, remote_hs.peer_id))
        }
    }

    async fn complete_mse_handshake(
        mut stream: tokio::net::TcpStream,
        _info_hash: &[u8; 20],
        remote_hs: &Handshake,
        _require_encryption: bool,
    ) -> Result<Self, String> {
        let mut initiator = MseHandshake::new_initiator();

        let step1_i = initiator.build_step1();
        stream.write_all(&step1_i).await
            .map_err(|e| format!("MSE Step1 send failed: {}", e))?;
        stream.flush().await
            .map_err(|e| format!("MSE Step1 flush failed: {}", e))?;

        let mut step1_r_buf = vec![0u8; step1_i.len()];
        match stream.read_exact(&mut step1_r_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof || e.to_string().contains("eof") => {
                return Err("MSE Step1: peer closed connection".to_string());
            }
            Err(e) => return Err(format!("MSE Step1 read failed: {}", e)),
        }

        initiator.receive_step1(&step1_r_buf)?;

        let step2_i = initiator.build_step2()?;
        stream.write_all(&step2_i).await
            .map_err(|e| format!("MSE Step2 send failed: {}", e))?;
        stream.flush().await
            .map_err(|e| format!("MSE Step2 flush failed: {}", e))?;

        let mut step2_r_buf = vec![0u8; step2_i.len()];
        match stream.read_exact(&mut step2_r_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err("MSE Step2: peer closed connection".to_string());
            }
            Err(e) => return Err(format!("MSE Step2 read failed: {}", e)),
        }

        let _method = initiator.receive_step2(&step2_r_buf)?;
        let crypto = initiator.finalize()?;

        info!("MSE握手完成: encrypted={}", crypto.is_encrypted());

        let peer_id = remote_hs.peer_id;
        let conn = PeerConnection::from_stream_with_peer(stream, peer_id);

        Ok(Self {
            inner: conn,
            crypto,
            mse_negotiated: true,
        })
    }

    fn from_plain_connection(stream: tokio::net::TcpStream, peer_id: [u8; 20]) -> Self {
        let conn = PeerConnection::from_stream_with_peer(stream, peer_id);
        EncryptedConnection {
            inner: conn,
            crypto: MseCryptoState::new_plain(),
            mse_negotiated: false,
        }
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

    pub async fn read_message(&mut self) -> Result<Option<BtMessage>, String> {
        use crate::bittorrent::message::factory::parse_message;

        let mut len_buf = [0u8; 4];
        match self.read_encrypted_exact(&mut len_buf).await {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => return Err(e),
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 {
            return Ok(Some(BtMessage::KeepAlive));
        }

        let mut payload_buf = vec![0u8; msg_len];
        self.read_encrypted_exact(&mut payload_buf).await?;

        let mut full_msg = vec![0u8; 4 + msg_len];
        full_msg[0..4].copy_from_slice(&len_buf);
        full_msg[4..].copy_from_slice(&payload_buf);

        parse_message(&full_msg)
    }

    async fn send_encrypted(&mut self, data: &[u8]) -> Result<(), String> {
        let mut buf = data.to_vec();
        self.crypto.encrypt(&mut buf);

        self.inner.stream_write(&buf).await?;
        self.inner.stream_flush().await?;

        debug!("发送加密消息: {} bytes", buf.len());
        Ok(())
    }

    async fn read_encrypted_exact(&mut self, buf: &mut [u8]) -> Result<bool, String> {
        match self.inner.stream_read_exact(buf).await {
            Ok(_) => {
                self.crypto.decrypt(buf);
                Ok(true)
            }
            Err(e) => {
                if e.contains("unexpected eof") || e.contains("failed to fill whole buffer") {
                    Ok(false)
                } else {
                    Err(format!("读取加密消息失败: {}", e))
                }
            }
        }
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
        self.send_message(&BtMessage::Request { request: req }).await
    }

    pub async fn send_cancel(&mut self, req: &PieceBlockRequest) -> Result<(), String> {
        self.inner.state.remove_request(req);
        self.send_message(&BtMessage::Cancel { request: req.clone() }).await
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<(), String> {
        self.inner.remote_bitfield = bitfield.clone();
        self.send_message(&BtMessage::Bitfield { data: bitfield }).await
    }

    pub fn state(&self) -> &PeerState {
        &self.inner.state
    }

    pub fn remote_peer_id(&self) -> Option<&[u8; 20]> {
        self.inner.remote_peer_id.as_ref()
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
        let enc = MseCryptoState::new_encrypted(
            &crate::bittorrent::extension::mse_crypto::MseDerivedKeys::derive(b"test"),
            true,
        );
        assert!(enc.is_encrypted());

        let plain = MseCryptoState::new_plain();
        assert!(!plain.is_encrypted());
    }

    #[test]
    fn test_should_negotiate_all_combos() {
        assert!(!MseHandshake::should_negotiate(true, &[0x00]));
        assert!(MseHandshake::should_negotiate(true, &[0x01]));
        assert!(MseHandshake::should_negotiate(true, &[0xFF]));
        assert!(!MseHandshake::should_negotiate(false, &[0x01]));
        assert!(!MseHandshake::should_negotiate(true, &[]));
    }

    #[tokio::test]
    async fn test_connect_unreachable_returns_err() {
        let result = EncryptedConnection::connect_with_mse(
            &PeerAddr::new("127.0.0.1", 1),
            &[0xAB; 20],
            false,
        ).await;
        assert!(result.is_err(), "unreachable address should fail");
    }
}
