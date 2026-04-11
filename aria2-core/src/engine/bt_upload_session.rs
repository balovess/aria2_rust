use tracing::{debug, warn};

use crate::error::Result;
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::RateLimiterConfig;

use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;

pub trait PieceDataProvider: Send + Sync {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>>;
    fn has_piece(&self, piece_index: u32) -> bool;
    fn num_pieces(&self) -> u32;
}

pub struct BtSeedingConfig {
    pub max_upload_bytes_per_sec: Option<u64>,
    pub max_peers_to_unchoke: usize,
    pub optimistic_unchoke_interval_secs: u64,
}

impl Default for BtSeedingConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes_per_sec: None,
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        }
    }
}

pub struct BtUploadSession {
    conn: PeerConnection,
    am_choke_state: bool,
    peer_interested: bool,
    uploaded_bytes: u64,
    upload_limiter: Option<RateLimiter>,
    pub(crate) is_dead: bool,
}

impl BtUploadSession {
    pub fn new(conn: PeerConnection, config: &BtSeedingConfig) -> Self {
        let upload_limiter = config
            .max_upload_bytes_per_sec
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(None, Some(r))));

        Self {
            conn,
            am_choke_state: false,
            peer_interested: false,
            uploaded_bytes: 0,
            upload_limiter,
            is_dead: false,
        }
    }

    pub async fn handle_incoming_messages(
        &mut self,
        provider: &dyn PieceDataProvider,
    ) -> Result<u64> {
        if self.is_dead {
            return Ok(0);
        }

        let round_uploaded = self.uploaded_bytes;

        loop {
            match self.conn.read_message().await {
                Ok(Some(msg)) => match msg {
                    BtMessage::Request { request } => {
                        if !self.am_choke_state && self.peer_interested {
                            debug!(
                                "Upload request: piece={}, offset={}, len={}",
                                request.index, request.begin, request.length
                            );

                            let data = provider.get_piece_data(
                                request.index,
                                request.begin,
                                request.length,
                            );
                            if let Some(piece_data) = data {
                                let data_len = piece_data.len() as u64;
                                if let Some(ref lim) = self.upload_limiter {
                                    lim.acquire_upload(data_len).await;
                                }
                                self.conn.send_message(&BtMessage::Piece {
                                        index: request.index,
                                        begin: request.begin,
                                        data: piece_data,
                                    }).await.map_err(|e| crate::error::Aria2Error::Recoverable(
                                        crate::error::RecoverableError::TemporaryNetworkFailure { message: e }
                                    ))?;
                                self.uploaded_bytes += data_len;
                            } else {
                                warn!(
                                    "No data for piece {} at offset {}",
                                    request.index, request.begin
                                );
                            }
                        } else {
                            debug!(
                                "Ignoring request: choked={} interested={}",
                                self.am_choke_state, self.peer_interested
                            );
                        }
                    }
                    BtMessage::Interested => {
                        self.peer_interested = true;
                        if !self.am_choke_state {
                            self.conn.send_unchoke().await.ok();
                        }
                    }
                    BtMessage::NotInterested => {
                        self.peer_interested = false;
                    }
                    BtMessage::Choke => {
                        debug!("Peer choked us");
                    }
                    BtMessage::Unchoke => {
                        debug!("Peer unchoked us");
                    }
                    BtMessage::Have { piece_index } => {
                        debug!("Peer has piece {}", piece_index);
                    }
                    BtMessage::Cancel { request } => {
                        debug!(
                            "Peer cancelled request for piece {} offset {}",
                            request.index, request.begin
                        );
                    }
                    BtMessage::Piece { .. } => {
                        debug!("Unexpected Piece from peer during seeding");
                    }
                    BtMessage::Bitfield { .. } => {}
                    BtMessage::KeepAlive => {}
                    BtMessage::Port { port: _ } => {}
                    BtMessage::AllowedFast { index } => {
                        debug!("Received AllowedFast for piece {}", index);
                    }
                    BtMessage::Reject {
                        index,
                        offset,
                        length,
                    } => {
                        debug!(
                            "Received Reject for piece {} offset {} len {}",
                            index, offset, length
                        );
                    }
                    BtMessage::Suggest { index } => {
                        debug!("Received Suggest for piece {}", index);
                    }
                    BtMessage::HaveAll => {
                        debug!("Received HaveAll");
                    }
                    BtMessage::HaveNone => {
                        debug!("Received HaveNone");
                    }
                },
                Ok(None) => {
                    debug!("EOF from peer, marking session as dead");
                    self.is_dead = true;
                    break;
                }
                Err(e) => {
                    warn!("Read error from upload peer: {}, marking dead", e);
                    self.is_dead = true;
                    break;
                }
            }
        }

        Ok(self.uploaded_bytes - round_uploaded)
    }

    pub async fn unchoke_peer(&mut self) -> Result<()> {
        if self.am_choke_state {
            self.conn.send_unchoke().await.map_err(|e| {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::TemporaryNetworkFailure { message: e },
                )
            })?;
            self.am_choke_state = false;
        }
        Ok(())
    }

    pub async fn choke_peer(&mut self) -> Result<()> {
        if !self.am_choke_state {
            self.conn.send_choke().await.map_err(|e| {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::TemporaryNetworkFailure { message: e },
                )
            })?;
            self.am_choke_state = true;
        }
        Ok(())
    }

    pub fn is_peer_choked(&self) -> bool {
        self.am_choke_state
    }

    pub fn is_peer_interested(&self) -> bool {
        self.peer_interested
    }

    pub fn is_dead(&self) -> bool {
        self.is_dead
    }

    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    pub fn connection_mut(&mut self) -> &mut PeerConnection {
        &mut self.conn
    }
}

pub struct InMemoryPieceProvider {
    pieces: Vec<Option<Vec<u8>>>,
    piece_length: u32,
}

impl InMemoryPieceProvider {
    pub fn new(piece_length: u32, num_pieces: u32) -> Self {
        let mut pieces = Vec::with_capacity(num_pieces as usize);
        for _ in 0..num_pieces {
            pieces.push(None);
        }
        Self {
            pieces,
            piece_length,
        }
    }

    pub fn set_piece_data(&mut self, index: u32, data: Vec<u8>) {
        if (index as usize) < self.pieces.len() {
            self.pieces[index as usize] = Some(data);
        }
    }

    pub fn set_all_from_pattern<F>(&mut self, f: F)
    where
        F: Fn(u32, u32) -> u8,
    {
        for i in 0..self.pieces.len() {
            let len = if i == self.pieces.len() - 1 {
                let total = self.piece_length as usize * (self.pieces.len() - 1);
                1024 * 100 - total
            } else {
                self.piece_length as usize
            };
            let mut data = Vec::with_capacity(len);
            for j in 0..len {
                data.push(f(i as u32, j as u32));
            }
            self.pieces[i] = Some(data);
        }
    }
}

impl PieceDataProvider for InMemoryPieceProvider {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        let piece = self.pieces.get(piece_index as usize)?.as_ref()?;
        let start = offset as usize;
        let end = (start + length as usize).min(piece.len());
        if start >= piece.len() {
            return None;
        }
        Some(piece[start..end].to_vec())
    }

    fn has_piece(&self, piece_index: u32) -> bool {
        self.pieces
            .get(piece_index as usize)
            .is_some_and(|p| p.is_some())
    }

    fn num_pieces(&self) -> u32 {
        self.pieces.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seeding_config_default() {
        let cfg = BtSeedingConfig::default();
        assert!(cfg.max_upload_bytes_per_sec.is_none());
        assert_eq!(cfg.max_peers_to_unchoke, 4);
        assert_eq!(cfg.optimistic_unchoke_interval_secs, 30);
    }

    #[test]
    fn test_in_memory_provider_creation() {
        let provider = InMemoryPieceProvider::new(16384, 10);
        assert_eq!(provider.num_pieces(), 10);
        assert!(!provider.has_piece(0));
        assert!(provider.get_piece_data(0, 0, 100).is_none());
    }

    #[test]
    fn test_in_memory_provider_set_and_get() {
        let mut provider = InMemoryPieceProvider::new(256, 4);
        provider.set_piece_data(0, vec![0xAB; 256]);
        provider.set_piece_data(2, vec![0xCD; 128]);

        assert!(provider.has_piece(0));
        assert!(!provider.has_piece(1));
        assert!(provider.has_piece(2));

        let data = provider.get_piece_data(0, 10, 50).unwrap();
        assert_eq!(data.len(), 50);
        assert!(data.iter().all(|&b| b == 0xAB));

        let partial = provider.get_piece_data(2, 100, 28).unwrap();
        assert_eq!(partial.len(), 28);
        assert!(partial.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn test_in_memory_provider_set_all_from_pattern() {
        let mut provider = InMemoryPieceProvider::new(100, 5);
        provider.set_all_from_pattern(|piece_idx, byte_idx| {
            ((piece_idx * 37 + byte_idx * 13) % 256) as u8
        });

        for i in 0..5u32 {
            assert!(provider.has_piece(i));
            let data = provider.get_piece_data(i, 0, 100).unwrap();
            for (j, &byte) in data.iter().enumerate() {
                assert_eq!(byte, ((i * 37 + j as u32 * 13) % 256) as u8);
            }
        }
    }

    #[test]
    fn test_in_memory_provider_offset_beyond_piece() {
        let mut provider = InMemoryPieceProvider::new(50, 2);
        provider.set_piece_data(0, vec![0x42; 50]);

        assert!(provider.get_piece_data(0, 40, 20).is_some());
        assert!(provider.get_piece_data(0, 60, 10).is_none());
        assert!(provider.get_piece_data(99, 0, 10).is_none());
    }

    #[test]
    fn test_in_memory_provider_last_piece_smaller() {
        let total_size = 260u32;
        let piece_len = 100u32;
        let num_pieces = (total_size + piece_len - 1) / piece_len;
        let mut provider = InMemoryPieceProvider::new(piece_len, num_pieces);

        provider.set_all_from_pattern(|_, idx| idx as u8);

        assert!(provider.has_piece(0));
        assert!(provider.has_piece(1));
        assert!(provider.has_piece(2));

        let last_piece = provider.get_piece_data(2, 0, 60).unwrap();
        assert_eq!(last_piece.len(), 60);
    }
}
