use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::state::PeerState;
use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::message::types::{BtMessage, PieceBlockRequest};

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

    pub fn to_socket_addr(&self) -> Result<std::net::SocketAddr, std::net::AddrParseError> {
        self.ip
            .parse()
            .map(|ip| std::net::SocketAddr::new(ip, self.port))
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
    write_timeout: Option<std::time::Duration>,
    write_failed: bool,
    remote_addr: Option<std::net::SocketAddr>,
    state: PeerState,
    remote_peer_id: Option<[u8; 20]>,
    /// Whether the remote BitTorrent handshake advertised BEP 5 DHT support.
    remote_supports_dht: bool,
    /// Whether the remote BitTorrent handshake advertised BEP 6 support.
    remote_supports_fast_extension: bool,
    /// Whether the remote BitTorrent handshake advertised BEP 10 support.
    remote_supports_extended_messaging: bool,
    /// MSE is negotiated at connection setup; `None` means plaintext TCP.
    crypto: Option<MseCryptoState>,
    /// Encrypted bytes read past the MSE handshake, awaiting message decoding.
    read_ahead: Vec<u8>,
    // Keep partially received frames across cancellation of read_message.
    read_buffer: BytesMut,
}

impl PeerConnection {
    /// Complete a peer connection over a TCP stream selected by the caller.
    pub async fn connect_with_stream(
        mut stream: TcpStream,
        socket_addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
    ) -> Result<Self, String> {
        debug!("Connecting to peer over selected stream: {}", socket_addr);

        let mut handshake = Handshake::new(info_hash_v1, local_peer_id).with_dht(dht_enabled);
        handshake.set_bep52_enabled(info_hash_v2.is_some());
        stream
            .write_all(&handshake.to_bytes())
            .await
            .map_err(|e| format!("Failed to send handshake: {}", e))?;

        let remote_hs = match info_hash_v2 {
            Some(info_hash_v2) => {
                Self::read_remote_handshake_hybrid(&mut stream, info_hash_v1, info_hash_v2, timeout)
                    .await?
            }
            None => Self::read_remote_handshake(&mut stream, info_hash_v1, timeout).await?,
        };
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

    async fn read_remote_handshake_hybrid(
        stream: &mut tokio::net::TcpStream,
        info_hash_v1: &[u8; 20],
        info_hash_v2: &[u8; 32],
        timeout: std::time::Duration,
    ) -> Result<Handshake, String> {
        let mut response = [0u8; 68];
        tokio::time::timeout(timeout, stream.read_exact(&mut response))
            .await
            .map_err(|_| "Handshake response timeout".to_string())?
            .map_err(|e| format!("Failed to read handshake response: {}", e))?;

        let remote_hs = Handshake::parse(&response)?;
        let v2_truncated: [u8; 20] = info_hash_v2[..20]
            .try_into()
            .expect("SHA-256 hash is 32 bytes");
        if remote_hs.info_hash == *info_hash_v1 {
            return Ok(remote_hs);
        }
        if remote_hs.info_hash == v2_truncated && remote_hs.supports_bep52() {
            return Ok(remote_hs);
        }
        Err("hybrid handshake info_hash mismatch or missing BEP 52 capability".to_string())
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
            write_timeout: None,
            write_failed: false,
            remote_addr,
            state: PeerState::new(),
            remote_peer_id: Some(remote_hs.peer_id),
            remote_supports_dht: remote_hs.supports_dht(),
            remote_supports_fast_extension: remote_hs.supports_fast_extension(),
            remote_supports_extended_messaging: remote_hs.supports_extended_messaging(),
            crypto: None,
            read_ahead: Vec::new(),
            read_buffer: BytesMut::new(),
        })
    }

    /// Wrap a stream after an external handshake has already captured the
    /// remote BEP 5 capability.
    pub fn from_stream_with_peer(
        stream: tokio::net::TcpStream,
        peer_id: [u8; 20],
        remote_supports_dht: bool,
        remote_supports_fast_extension: bool,
    ) -> Self {
        Self::from_stream_with_peer_capabilities(
            stream,
            peer_id,
            remote_supports_dht,
            remote_supports_fast_extension,
            false,
        )
    }

    /// Wrap a stream after an external handshake while preserving the remote
    /// peer's advertised protocol capabilities.
    pub fn from_stream_with_peer_capabilities(
        stream: tokio::net::TcpStream,
        peer_id: [u8; 20],
        remote_supports_dht: bool,
        remote_supports_fast_extension: bool,
        remote_supports_extended_messaging: bool,
    ) -> Self {
        let remote_addr = stream.peer_addr().ok();
        Self {
            stream,
            write_timeout: None,
            write_failed: false,
            remote_addr,
            state: PeerState::new(),
            remote_peer_id: Some(peer_id),
            remote_supports_dht,
            remote_supports_fast_extension,
            remote_supports_extended_messaging,
            crypto: None,
            read_ahead: Vec::new(),
            read_buffer: BytesMut::new(),
        }
    }

    pub(crate) fn from_stream_with_mse(
        stream: TcpStream,
        crypto: MseCryptoState,
        read_ahead: Vec<u8>,
        peer_id: [u8; 20],
        remote_supports_dht: bool,
        remote_supports_fast_extension: bool,
        remote_supports_extended_messaging: bool,
    ) -> Self {
        let mut connection = Self::from_stream_with_peer_capabilities(
            stream,
            peer_id,
            remote_supports_dht,
            remote_supports_fast_extension,
            remote_supports_extended_messaging,
        );
        connection.crypto = Some(crypto);
        connection.read_ahead = read_ahead;
        connection
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
        if self.write_failed {
            return Err("Peer socket is unusable after a failed write".to_owned());
        }
        let result = if let Some(crypto) = &mut self.crypto {
            let mut encrypted = data.to_vec();
            crypto.encrypt(&mut encrypted);
            self.write_with_progress_timeout(&encrypted).await
        } else {
            self.write_with_progress_timeout(data).await
        };
        if result.is_err() {
            // A failed write may have emitted only a prefix of a wire frame;
            // never append another frame to that potentially corrupt stream.
            self.write_failed = true;
        }
        result
    }

    /// Bound a period with no socket write progress. Each successful partial
    /// write starts a fresh interval, so slow but advancing peers are not
    /// penalized for the total size of a queued upload.
    async fn write_with_progress_timeout(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(write_timeout) = self.write_timeout else {
            self.stream
                .write_all(data)
                .await
                .map_err(|error| format!("Failed to send message: {error}"))?;
            return self
                .stream
                .flush()
                .await
                .map_err(|error| format!("Failed to flush buffer: {error}"));
        };
        let mut written = 0;
        while written < data.len() {
            let count = tokio::time::timeout(write_timeout, self.stream.write(&data[written..]))
                .await
                .map_err(|_| "Timed out waiting for peer socket write progress".to_owned())?
                .map_err(|error| format!("Failed to send message: {error}"))?;
            if count == 0 {
                return Err("Failed to send message: peer socket closed".to_owned());
            }
            written += count;
        }
        tokio::time::timeout(write_timeout, self.stream.flush())
            .await
            .map_err(|_| "Timed out flushing peer socket".to_owned())?
            .map_err(|error| format!("Failed to flush buffer: {error}"))
    }

    /// Configure the maximum time a peer socket write may make no progress.
    pub fn set_write_timeout(&mut self, timeout: std::time::Duration) {
        self.write_timeout = Some(timeout);
    }

    pub async fn read_message(&mut self) -> Result<Option<BtMessage>, String> {
        if !self.read_ahead.is_empty() {
            let mut pending = std::mem::take(&mut self.read_ahead);
            if let Some(crypto) = &mut self.crypto {
                crypto.decrypt(&mut pending);
            }
            self.read_buffer.extend_from_slice(&pending);
        }

        loop {
            if self.read_buffer.len() >= 4 {
                let msg_len =
                    u32::from_be_bytes(self.read_buffer[..4].try_into().unwrap()) as usize;
                if msg_len > crate::bittorrent::message::MAX_BT_MESSAGE_LENGTH {
                    return Err(format!(
                        "BitTorrent message length {} exceeds maximum {}",
                        msg_len,
                        crate::bittorrent::message::MAX_BT_MESSAGE_LENGTH
                    ));
                }
                let frame_len = 4 + msg_len;
                if self.read_buffer.len() >= frame_len {
                    let frame = self.read_buffer.split_to(frame_len).freeze();
                    if msg_len == 0 {
                        self.state.mark_message_received();
                        return Ok(Some(BtMessage::KeepAlive));
                    }
                    if let Some(message) =
                        crate::bittorrent::message::factory::parse_message_bytes(frame)?
                    {
                        self.state.mark_message_received();
                        return Ok(Some(message));
                    }
                    continue;
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
            if let Some(crypto) = &mut self.crypto {
                crypto.decrypt(&mut chunk[..bytes_read]);
            }
            self.read_buffer.extend_from_slice(&chunk[..bytes_read]);
        }
    }

    pub async fn send_choke(&mut self) -> Result<(), String> {
        self.state.set_am_choking(true);
        self.send_message(&BtMessage::Choke).await
    }

    pub async fn send_unchoke(&mut self) -> Result<(), String> {
        self.state.set_am_choking(false);
        self.send_message(&BtMessage::Unchoke).await
    }

    pub async fn send_interested(&mut self) -> Result<(), String> {
        self.state.set_am_interested(true);
        self.send_message(&BtMessage::Interested).await
    }

    pub async fn send_not_interested(&mut self) -> Result<(), String> {
        self.state.set_am_interested(false);
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
        self.send_message(&BtMessage::Bitfield { data: bitfield })
            .await
    }

    pub fn is_connected(&self) -> bool {
        self.remote_peer_id.is_some()
    }

    pub fn state(&self) -> &PeerState {
        &self.state
    }

    pub fn remote_peer_id(&self) -> Option<&[u8; 20]> {
        self.remote_peer_id.as_ref()
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_addr
    }

    /// Whether the remote handshake advertised BEP 5 DHT support.
    pub fn remote_supports_dht(&self) -> bool {
        self.remote_supports_dht
    }

    /// Whether the remote BitTorrent handshake advertised BEP 6 support.
    pub fn remote_supports_fast_extension(&self) -> bool {
        self.remote_supports_fast_extension
    }

    /// Whether the remote BitTorrent handshake advertised BEP 10 support.
    pub fn remote_supports_extended_messaging(&self) -> bool {
        self.remote_supports_extended_messaging
    }

    pub fn is_mse_negotiated(&self) -> bool {
        self.crypto.is_some()
    }

    pub fn is_encrypted(&self) -> bool {
        self.crypto
            .as_ref()
            .is_some_and(MseCryptoState::is_encrypted)
    }
}

#[cfg(test)]
mod tests;
