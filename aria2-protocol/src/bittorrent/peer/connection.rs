use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::state::PeerState;
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use crate::bittorrent::peer::id;

#[derive(Debug, Clone)]
pub struct PeerAddr {
    pub ip: String,
    pub port: u16,
}

impl PeerAddr {
    pub fn new(ip: &str, port: u16) -> Self {
        Self {
            ip: ip.to_string(),
            port,
        }
    }

    pub fn from_compact(data: &[u8]) -> Option<Self> {
        if data.len() < 6 {
            return None;
        }
        let ip = format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some(Self { ip, port })
    }

    pub fn to_socket_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.ip, self.port)
            .parse()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
    }

    pub fn to_compact(&self) -> [u8; 6] {
        let mut buf = [0u8; 6];
        if let Ok(addr) = self.ip.parse::<std::net::Ipv4Addr>() {
            buf[..4].copy_from_slice(&addr.octets());
            buf[4..6].copy_from_slice(&self.port.to_be_bytes());
        }
        buf
    }
}

pub struct PeerConnection {
    stream: TcpStream,
    pub state: PeerState,
    pub remote_peer_id: Option<[u8; 20]>,
    pub remote_bitfield: Vec<u8>,
}

impl PeerConnection {
    pub async fn connect(addr: &PeerAddr, info_hash: &[u8; 20]) -> Result<Self, String> {
        let socket_addr = addr.to_socket_addr();
        debug!("Connecting to peer: {}", socket_addr);

        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::net::TcpStream::connect(&socket_addr),
        )
        .await
        .map_err(|_| format!("连接peer超时: {}", socket_addr))?
        .map_err(|e| format!("连接peer失败: {}", e))?;

        Self::from_stream(stream, info_hash).await
    }

    pub async fn from_stream(
        mut stream: tokio::net::TcpStream,
        info_hash: &[u8; 20],
    ) -> Result<Self, String> {
        let my_peer_id = id::generate_peer_id();
        let handshake = Handshake::new(info_hash, &my_peer_id);
        let handshake_bytes = handshake.to_bytes();

        stream
            .write_all(&handshake_bytes)
            .await
            .map_err(|e| format!("发送握手失败: {}", e))?;

        let mut response = [0u8; 68];
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read_exact(&mut response),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("读取握手响应失败: {}", e)),
            Err(_) => return Err("读取握手响应超时".to_string()),
        }

        let remote_hs = Handshake::parse(&response)?;

        if remote_hs.info_hash != *info_hash {
            return Err("info_hash不匹配".to_string());
        }

        info!("Peer握手成功: peer_id={}", remote_hs.peer_id_str());

        Ok(Self {
            stream,
            state: PeerState::new(),
            remote_peer_id: Some(remote_hs.peer_id),
            remote_bitfield: vec![],
        })
    }

    pub fn from_stream_with_peer(stream: tokio::net::TcpStream, peer_id: [u8; 20]) -> Self {
        Self {
            stream,
            state: PeerState::new(),
            remote_peer_id: Some(peer_id),
            remote_bitfield: vec![],
        }
    }

    pub async fn send_message(&mut self, message: &BtMessage) -> Result<(), String> {
        use crate::bittorrent::message::serializer::serialize;
        let data = serialize(message);

        self.stream
            .write_all(&data)
            .await
            .map_err(|e| format!("发送消息失败: {}", e))?;
        self.stream
            .flush()
            .await
            .map_err(|e| format!("刷新缓冲区失败: {}", e))?;

        debug!("发送消息: {:?}", message.message_id());
        Ok(())
    }

    pub async fn read_message(&mut self) -> Result<Option<BtMessage>, String> {
        use crate::bittorrent::message::factory::parse_message;

        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(format!("读取消息长度失败: {}", e)),
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 {
            return Ok(Some(BtMessage::KeepAlive));
        }

        let mut payload_buf = vec![0u8; msg_len];
        self.stream
            .read_exact(&mut payload_buf)
            .await
            .map_err(|e| format!("读取消息体失败: {}", e))?;

        let mut full_msg = vec![0u8; 4 + msg_len];
        full_msg[0..4].copy_from_slice(&len_buf);
        full_msg[4..].copy_from_slice(&payload_buf);

        parse_message(&full_msg)
    }

    pub async fn send_choke(&mut self) -> Result<(), String> {
        self.state.am_choking = true;
        self.send_message(&BtMessage::Choke).await
    }

    pub async fn send_unchoke(&mut self) -> Result<(), String> {
        self.state.am_choking = false;
        self.send_message(&BtMessage::Unchoke).await
    }

    pub async fn send_interested(&mut self) -> Result<(), String> {
        self.state.am_interested = true;
        self.send_message(&BtMessage::Interested).await
    }

    pub async fn send_not_interested(&mut self) -> Result<(), String> {
        self.state.am_interested = false;
        self.send_message(&BtMessage::NotInterested).await
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<(), String> {
        self.send_message(&BtMessage::Have { piece_index }).await
    }

    pub async fn send_request(&mut self, req: PieceBlockRequest) -> Result<(), String> {
        self.state.add_request(req.clone());
        self.send_message(&BtMessage::Request { request: req })
            .await
    }

    pub async fn send_cancel(&mut self, req: &PieceBlockRequest) -> Result<(), String> {
        self.state.remove_request(req);
        self.send_message(&BtMessage::Cancel {
            request: req.clone(),
        })
        .await
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<(), String> {
        self.remote_bitfield = bitfield.clone();
        self.send_message(&BtMessage::Bitfield { data: bitfield })
            .await
    }

    pub fn is_connected(&self) -> bool {
        self.remote_peer_id.is_some()
    }

    pub async fn stream_write(&mut self, data: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| format!("Stream write failed: {}", e))
    }

    pub async fn stream_flush(&mut self) -> Result<(), String> {
        self.stream
            .flush()
            .await
            .map_err(|e| format!("Stream flush failed: {}", e))
    }

    pub async fn stream_read_exact(&mut self, buf: &mut [u8]) -> Result<(), String> {
        self.stream
            .read_exact(buf)
            .await
            .map(|_| ())
            .map_err(|e| format!("Stream read failed: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_addr_compact_roundtrip() {
        let addr = PeerAddr::new("192.168.1.100", 6881);
        let compact = addr.to_compact();
        let parsed = PeerAddr::from_compact(&compact).unwrap();
        assert_eq!(parsed.ip, addr.ip);
        assert_eq!(parsed.port, addr.port);
    }

    #[test]
    fn test_peer_addr_from_compact() {
        let data: [u8; 6] = [127, 0, 0, 1, 0x1A, 0x0B];
        let addr = PeerAddr::from_compact(&data).unwrap();
        assert_eq!(addr.ip, "127.0.0.1");
        assert_eq!(addr.port, 6667);
    }

    #[test]
    fn test_peer_addr_too_short() {
        assert!(PeerAddr::from_compact(&[1, 2, 3]).is_none());
    }
}
