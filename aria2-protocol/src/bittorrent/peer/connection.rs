use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::state::PeerState;
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use crate::bittorrent::peer::id;

#[derive(Debug, Clone, PartialEq)]
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

    /// Compact peer format sizes for IPv4 and IPv6.
    pub const COMPACT_SIZE_V4: usize = 6;
    pub const COMPACT_SIZE_V6: usize = 18;

    /// Decode from IPv4 compact format (4-byte IP + 2-byte port = 6 bytes).
    pub fn from_compact(data: &[u8]) -> Option<Self> {
        if data.len() < Self::COMPACT_SIZE_V4 {
            return None;
        }
        let ip = format!("{}.{}.{}.{}", data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some(Self { ip, port })
    }

    /// Decode from IPv6 compact format (16-byte IP + 2-byte port = 18 bytes).
    pub fn from_compact_v6(data: &[u8]) -> Option<Self> {
        if data.len() < Self::COMPACT_SIZE_V6 {
            return None;
        }
        let ip_bytes: [u8; 16] = data[..16].try_into().ok()?;
        let ipv6 = std::net::Ipv6Addr::from(ip_bytes);
        let port = u16::from_be_bytes([data[16], data[17]]);
        Some(Self {
            ip: ipv6.to_string(),
            port,
        })
    }

    pub fn to_socket_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.ip, self.port)
            .parse()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap())
    }

    /// Encode to IPv4 compact format (4-byte IP + 2-byte port = 6 bytes).
    pub fn to_compact(&self) -> [u8; 6] {
        let mut buf = [0u8; 6];
        if let Ok(addr) = self.ip.parse::<std::net::Ipv4Addr>() {
            buf[..4].copy_from_slice(&addr.octets());
            buf[4..6].copy_from_slice(&self.port.to_be_bytes());
        }
        buf
    }

    /// Encode to IPv6 compact format (16-byte IP + 2-byte port = 18 bytes).
    pub fn to_compact_v6(&self) -> Option<[u8; 18]> {
        let addr = self.ip.parse::<std::net::Ipv6Addr>().ok()?;
        let mut buf = [0u8; 18];
        buf[..16].copy_from_slice(&addr.octets());
        buf[16..18].copy_from_slice(&self.port.to_be_bytes());
        Some(buf)
    }
}

pub struct PeerConnection {
    stream: TcpStream,
    remote_addr: Option<std::net::SocketAddr>,
    pub state: PeerState,
    pub remote_peer_id: Option<[u8; 20]>,
    pub remote_bitfield: Vec<u8>,
    // Keep partially received frames across cancellation of read_message.
    read_buffer: BytesMut,
}

impl PeerConnection {
    pub async fn connect_with_timeout(
        addr: &PeerAddr,
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let socket_addr = addr.to_socket_addr();
        debug!("Connecting to peer: {}", socket_addr);

        let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&socket_addr))
            .await
            .map_err(|_| format!("Peer connection timeout: {}", socket_addr))?
            .map_err(|e| format!("Peer connection failed: {}", e))?;

        Self::from_stream_with_timeout(stream, info_hash, local_peer_id, timeout).await
    }

    pub async fn connect(addr: &PeerAddr, info_hash: &[u8; 20]) -> Result<Self, String> {
        let local_peer_id = id::generate_peer_id();
        Self::connect_with_timeout(
            addr,
            info_hash,
            &local_peer_id,
            std::time::Duration::from_secs(15),
        )
        .await
    }

    async fn from_stream_with_timeout(
        mut stream: tokio::net::TcpStream,
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let handshake = Handshake::new(info_hash, local_peer_id);
        stream
            .write_all(&handshake.to_bytes())
            .await
            .map_err(|e| format!("Failed to send handshake: {}", e))?;

        let remote_hs = Self::read_remote_handshake(&mut stream, info_hash, timeout).await?;
        Self::finish_handshake(stream, remote_hs)
    }

    pub async fn from_stream(
        stream: tokio::net::TcpStream,
        info_hash: &[u8; 20],
    ) -> Result<Self, String> {
        let my_peer_id = id::generate_peer_id();
        Self::from_stream_with_timeout(
            stream,
            info_hash,
            &my_peer_id,
            std::time::Duration::from_secs(30),
        )
        .await
    }

    /// Complete the server side of a BitTorrent handshake.
    ///
    /// Incoming peers send their handshake first.  The listener must validate
    /// the torrent identity before admitting the endpoint to PeerStorage, then
    /// reply with our handshake and keep the stream for the upload/download
    /// session.
    pub async fn from_incoming_stream(
        mut stream: tokio::net::TcpStream,
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
    ) -> Result<Self, String> {
        let mut response = [0u8; 68];
        read_exact_with_timeout(&mut stream, &mut response).await?;
        Self::from_incoming_handshake(stream, response, info_hash, local_peer_id).await
    }

    /// Complete a plain incoming handshake when the first 20 bytes were
    /// already consumed by a shared listener for route selection.
    pub async fn from_incoming_stream_with_prefix(
        mut stream: tokio::net::TcpStream,
        prefix: [u8; 20],
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
    ) -> Result<Self, String> {
        let mut response = [0u8; 68];
        response[..20].copy_from_slice(&prefix);
        read_exact_with_timeout(&mut stream, &mut response[20..]).await?;
        Self::from_incoming_handshake(stream, response, info_hash, local_peer_id).await
    }

    /// Complete a plain incoming handshake after a shared listener consumed
    /// the complete handshake for routing.
    pub async fn from_incoming_handshake_bytes(
        stream: tokio::net::TcpStream,
        response: [u8; 68],
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
    ) -> Result<Self, String> {
        Self::from_incoming_handshake(stream, response, info_hash, local_peer_id).await
    }

    async fn from_incoming_handshake(
        mut stream: tokio::net::TcpStream,
        response: [u8; 68],
        info_hash: &[u8; 20],
        local_peer_id: &[u8; 20],
    ) -> Result<Self, String> {
        let remote_hs = Handshake::parse(&response)?;
        if remote_hs.info_hash != *info_hash {
            return Err("info_hash mismatch".to_string());
        }
        let handshake = Handshake::new(info_hash, local_peer_id);
        stream
            .write_all(&handshake.to_bytes())
            .await
            .map_err(|e| format!("Failed to send handshake response: {}", e))?;
        Self::finish_handshake(stream, remote_hs)
    }

    async fn read_remote_handshake(
        stream: &mut tokio::net::TcpStream,
        info_hash: &[u8; 20],
        timeout: std::time::Duration,
    ) -> Result<Handshake, String> {
        let mut response = [0u8; 68];
        match tokio::time::timeout(timeout, stream.read_exact(&mut response)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("Failed to read handshake response: {}", e)),
            Err(_) => return Err("Handshake response timeout".to_string()),
        }

        let remote_hs = Handshake::parse(&response)?;
        if remote_hs.info_hash != *info_hash {
            return Err("info_hash mismatch".to_string());
        }
        Ok(remote_hs)
    }

    fn finish_handshake(
        stream: tokio::net::TcpStream,
        remote_hs: Handshake,
    ) -> Result<Self, String> {
        info!(
            "Peer handshake successful: peer_id={}",
            remote_hs.peer_id_str()
        );
        let remote_addr = stream.peer_addr().ok();
        Ok(Self {
            stream,
            remote_addr,
            state: PeerState::new(),
            remote_peer_id: Some(remote_hs.peer_id),
            remote_bitfield: vec![],
            read_buffer: BytesMut::new(),
        })
    }

    pub fn from_stream_with_peer(stream: tokio::net::TcpStream, peer_id: [u8; 20]) -> Self {
        let remote_addr = stream.peer_addr().ok();
        Self {
            stream,
            remote_addr,
            state: PeerState::new(),
            remote_peer_id: Some(peer_id),
            remote_bitfield: vec![],
            read_buffer: BytesMut::new(),
        }
    }

    pub async fn send_message(&mut self, message: &BtMessage) -> Result<(), String> {
        use crate::bittorrent::message::serializer::serialize;
        let data = serialize(message);

        self.send_serialized(&data).await?;
        debug!("Sent message: {:?}", message.message_id());
        Ok(())
    }

    /// Send an already-framed BitTorrent message without serializing it again.
    ///
    /// Small control frames such as HAVE are broadcast to many peers. Keeping
    /// the frame at the caller avoids rebuilding the same nine bytes once per
    /// connection while preserving the connection's write/flush boundary.
    pub async fn send_serialized(&mut self, data: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| format!("Failed to send message: {}", e))?;
        self.stream
            .flush()
            .await
            .map_err(|e| format!("Failed to flush buffer: {}", e))?;
        Ok(())
    }

    pub async fn read_message(&mut self) -> Result<Option<BtMessage>, String> {
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

            let mut chunk = [0u8; 16 * 1024];
            let bytes_read = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| format!("Failed to read message: {}", e))?;
            if bytes_read == 0 {
                return if self.read_buffer.is_empty() {
                    Ok(None)
                } else {
                    Err("Failed to read message: unexpected eof".to_string())
                };
            }
            self.read_buffer.extend_from_slice(&chunk[..bytes_read]);
        }
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

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_addr
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
        while self.read_buffer.len() < buf.len() {
            let mut chunk = [0u8; 16 * 1024];
            let bytes_read = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| format!("Stream read failed: {}", e))?;
            if bytes_read == 0 {
                return Err("Stream read failed: unexpected eof".to_string());
            }
            self.read_buffer.extend_from_slice(&chunk[..bytes_read]);
        }

        buf.copy_from_slice(&self.read_buffer[..buf.len()]);
        let _ = self.read_buffer.split_to(buf.len());
        Ok(())
    }

    pub async fn stream_read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        self.stream
            .read(buf)
            .await
            .map_err(|e| format!("Stream read failed: {}", e))
    }
}

async fn read_exact_with_timeout(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
) -> Result<(), String> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stream.read_exact(buffer),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("Failed to read handshake: {error}")),
        Err(_) => Err("Handshake response timeout".to_string()),
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

    #[test]
    fn test_peer_addr_from_compact_v6() {
        // ::1 (loopback) + port 6881
        let mut data = [0u8; 18];
        data[15] = 1; // ::1 in 16 bytes
        data[16..18].copy_from_slice(&6881u16.to_be_bytes());
        let addr = PeerAddr::from_compact_v6(&data).unwrap();
        assert_eq!(addr.ip, "::1");
        assert_eq!(addr.port, 6881);
    }

    #[test]
    fn test_peer_addr_compact_v6_roundtrip() {
        let addr = PeerAddr::new("2001:db8::1", 6881);
        let compact = addr.to_compact_v6().unwrap();
        let parsed = PeerAddr::from_compact_v6(&compact).unwrap();
        assert_eq!(parsed.ip, addr.ip);
        assert_eq!(parsed.port, addr.port);
    }

    #[test]
    fn test_peer_addr_compact_v6_too_short() {
        assert!(PeerAddr::from_compact_v6(&[0u8; 17]).is_none());
    }

    #[test]
    fn test_peer_addr_to_compact_v6_non_ipv6() {
        let addr = PeerAddr::new("192.168.1.1", 6881);
        assert!(addr.to_compact_v6().is_none());
    }

    #[test]
    fn test_peer_addr_from_compact_v6_full_addr() {
        // 2001:0db8:85a3:0000:0000:8a2e:0370:7334 + port 1234
        let mut data = [0u8; 18];
        data[0..2].copy_from_slice(&[0x20, 0x01]);
        data[2..4].copy_from_slice(&[0x0d, 0xb8]);
        data[4..6].copy_from_slice(&[0x85, 0xa3]);
        data[6..8].copy_from_slice(&[0x00, 0x00]);
        data[8..10].copy_from_slice(&[0x00, 0x00]);
        data[10..12].copy_from_slice(&[0x8a, 0x2e]);
        data[12..14].copy_from_slice(&[0x03, 0x70]);
        data[14..16].copy_from_slice(&[0x73, 0x34]);
        data[16..18].copy_from_slice(&1234u16.to_be_bytes());

        let addr = PeerAddr::from_compact_v6(&data).unwrap();
        assert_eq!(addr.ip, "2001:db8:85a3::8a2e:370:7334");
        assert_eq!(addr.port, 1234);
    }

    #[tokio::test]
    async fn test_read_message_preserves_partial_frame_after_cancellation() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut connection = PeerConnection::from_stream_with_peer(server, [0u8; 20]);
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

    #[tokio::test]
    async fn connect_with_timeout_uses_configured_handshake_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });

        let addr = PeerAddr::new("127.0.0.1", address.port());
        let result = PeerConnection::connect_with_timeout(
            &addr,
            &[1u8; 20],
            &[b'X'; 20],
            std::time::Duration::from_millis(20),
        )
        .await;

        match result {
            Err(error) => assert!(error.contains("Handshake response timeout")),
            Ok(_) => panic!("the peer does not answer the handshake"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn connect_with_timeout_sends_the_configured_peer_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 68];
            tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
                .await
                .unwrap();
            let received = Handshake::parse(&request).unwrap();
            assert_eq!(received.peer_id, [b'X'; 20]);
            let response = Handshake::new(&[1u8; 20], &[b'Y'; 20]).to_bytes();
            tokio::io::AsyncWriteExt::write_all(&mut stream, &response)
                .await
                .unwrap();
        });

        let addr = PeerAddr::new("127.0.0.1", address.port());
        let connection = PeerConnection::connect_with_timeout(
            &addr,
            &[1u8; 20],
            &[b'X'; 20],
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(connection.remote_peer_id, Some([b'Y'; 20]));
        server.await.unwrap();
    }
}
