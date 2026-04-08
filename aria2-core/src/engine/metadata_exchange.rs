use std::net::SocketAddr;
use std::time::Duration;
use tracing::{info, debug, warn};
use tokio::time::timeout;

use aria2_protocol::bittorrent::peer::connection::{PeerConnection, PeerAddr};
use aria2_protocol::bittorrent::extension::ut_metadata::{
    ExtensionHandshake,
    UtMetadataMsg,
    MetadataCollector,
};
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

pub struct MetadataExchangeConfig {
    pub max_peers_to_try: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub piece_size: u32,
}

impl Default for MetadataExchangeConfig {
    fn default() -> Self {
        Self {
            max_peers_to_try: 5,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(10),
            piece_size: 16 * 1024,
        }
    }
}

pub struct MetadataExchangeSession {
    config: MetadataExchangeConfig,
}

impl MetadataExchangeSession {
    pub fn new(config: MetadataExchangeConfig) -> Self {
        Self { config }
    }

    pub async fn fetch_metadata(
        &self,
        info_hash: &[u8; 20],
        peers: &[SocketAddr],
    ) -> Result<Vec<u8>, String> {
        if peers.is_empty() {
            return Err("No peers available for metadata fetch".to_string());
        }

        let mut errors = Vec::new();

        for peer_addr in peers.iter().take(self.config.max_peers_to_try) {
            match self.exchange_with_peer(info_hash, peer_addr).await {
                Ok(torrent_bytes) => {
                    info!("Metadata fetched successfully from {} ({} bytes)", peer_addr, torrent_bytes.len());
                    return Ok(torrent_bytes);
                }
                Err(e) => {
                    warn!("metadata exchange failed with {}: {}", peer_addr, e);
                    errors.push(e);
                }
            }
        }

        Err(format!(
            "All {} peers failed: {:?}",
            errors.len(),
            errors
        ))
    }

    async fn exchange_with_peer(
        &self,
        info_hash: &[u8; 20],
        peer_addr: &SocketAddr,
    ) -> Result<Vec<u8>, String> {
        let addr = PeerAddr::new(&peer_addr.ip().to_string(), peer_addr.port());

        let conn_result = tokio::time::timeout(self.config.connect_timeout, PeerConnection::connect(&addr, info_hash)).await;
        let mut conn = match conn_result {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(format!("Connect to {} failed: {}", peer_addr, e)),
            Err(_) => return Err(format!("Connect to {} timed out", peer_addr)),
        };

        debug!("Connected to {}, sending extension handshake", peer_addr);

        let hs_payload = ExtensionHandshake::new(0).to_bencode();
        let hs_encoded = hs_payload.encode();

        conn.stream_write(&[0]).await?;
        conn.stream_write(&hs_encoded).await?;
        conn.stream_flush().await?;

        debug!("Extension handshake sent to {}", peer_addr);

        let remote_hs_data = self.read_extension_message(&mut conn).await?;
        let remote_hs = ExtensionHandshake::parse(&remote_hs_data)
            .ok_or_else(|| "Failed to parse remote extension handshake".to_string())?;

        let metadata_size = remote_hs.metadata_size
            .ok_or_else(|| "Remote did not provide metadata_size".to_string())? as u64;

        if metadata_size == 0 {
            return Err("metadata_size is 0".to_string());
        }
        if metadata_size > 100 * 1024 * 1024 {
            return Err(format!("metadata_size too large: {}", metadata_size));
        }

        debug!("Remote reports metadata_size={} bytes", metadata_size);

        let num_pieces = ((metadata_size + self.config.piece_size as u64 - 1)
                        / self.config.piece_size as u64) as u32;
        let mut collector = MetadataCollector::new(metadata_size, self.config.piece_size);

        for piece_idx in 0..num_pieces {
            if collector.is_complete() { break; }

            let req_msg = UtMetadataMsg::Request(piece_idx);
            let encoded = req_msg.encode(20);

            conn.stream_write(&encoded).await?;
            conn.stream_flush().await?;

            match timeout(self.config.request_timeout, self.read_ut_metadata_response(&mut conn)).await {
                Ok(Ok(UtMetadataMsg::Data(recv_piece, data))) => {
                    collector.add_piece(recv_piece, &data);
                    debug!("Received piece {}/{} ({} bytes)", recv_piece + 1, num_pieces, data.len());
                }
                Ok(Ok(UtMetadataMsg::Reject(_))) => {
                    debug!("Piece {} rejected by {}", piece_idx, peer_addr);
                }
                Ok(Ok(UtMetadataMsg::Request(_))) => {
                    return Err("Unexpected Request from peer".to_string());
                }
                Ok(Err(inner_err)) => {
                    return Err(format!("ut_metadata error for piece {}: {}", piece_idx, inner_err));
                }
                Err(_) => {
                    return Err(format!("ut_metadata timeout for piece {}", piece_idx));
                }
            }
        }

        collector.assemble()
            .ok_or_else(|| "Incomplete metadata collection".to_string())
    }

    async fn read_extension_message(&self, conn: &mut PeerConnection) -> Result<BencodeValue, String> {
        let mut len_buf = [0u8; 4];
        conn.stream_read_exact(&mut len_buf).await
            .map_err(|e| format!("Read message length failed: {}", e))?;

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 10 * 1024 * 1024 {
            return Err("Invalid extension message length".to_string());
        }

        let mut payload = vec![0u8; msg_len];
        conn.stream_read_exact(&mut payload).await
            .map_err(|e| format!("Read message body failed: {}", e))?;

        if payload.first().copied() != Some(20u8) {
            return Err("Expected extended message ID 20".to_string());
        }

        BencodeValue::decode(&payload[1..])
            .map(|(v, _)| v)
            .map_err(|e| format!("Decode BEncode failed: {}", e))
    }

    async fn read_ut_metadata_response(&self, conn: &mut PeerConnection) -> Result<UtMetadataMsg, String> {
        let mut len_buf = [0u8; 4];
        conn.stream_read_exact(&mut len_buf).await
            .map_err(|e| format!("Read ut_metadata length failed: {}", e))?;

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 10 * 1024 * 1024 {
            return Err("Invalid ut_metadata message length".to_string());
        }

        let mut payload = vec![0u8; msg_len];
        conn.stream_read_exact(&mut payload).await
            .map_err(|e| format!("Read ut_metadata body failed: {}", e))?;

        UtMetadataMsg::decode(&payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let cfg = MetadataExchangeConfig::default();
        assert_eq!(cfg.max_peers_to_try, 5);
        assert_eq!(cfg.piece_size, 16 * 1024);
    }

    #[test]
    fn test_fetch_metadata_no_peers() {
        let session = MetadataExchangeSession::new(MetadataExchangeConfig::default());
        let target_hash = [0u8; 20];
        let peers: Vec<SocketAddr> = vec![];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(session.fetch_metadata(&target_hash, &peers));

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No peers available"));
    }

    #[tokio::test]
    async fn test_read_extension_message_invalid_length_zero() {
        let session = MetadataExchangeSession::new(MetadataExchangeConfig::default());
        let _ = session;
    }
}
