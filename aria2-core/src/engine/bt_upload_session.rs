use async_trait::async_trait;
use tracing::{debug, warn};

use crate::error::Result;
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::RateLimiterConfig;

use crate::engine::bt_message_validation::BtMessageValidator;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;

#[async_trait]
pub trait PieceDataProvider: Send + Sync {
    async fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>>;
    fn has_piece(&self, piece_index: u32) -> bool;
    fn num_pieces(&self) -> u32;
    fn piece_length(&self) -> u32;
}

pub struct BtSeedingConfig {
    pub max_upload_bytes_per_sec: Option<u64>,
    /// Process-wide limiter shared by all download/seeding commands.
    pub global_limiter: Option<RateLimiter>,
    pub max_peers_to_unchoke: usize,
    pub optimistic_unchoke_interval_secs: u64,
}

impl Default for BtSeedingConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes_per_sec: None,
            global_limiter: None,
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        }
    }
}

/// The per-peer upload state shared by downloading and seeding sessions.
///
/// A peer connection is duplex: while its download side is waiting for
/// `Piece` messages, the remote side may send `Interested` and `Request` for
/// pieces we already have. Keeping this state independent from the transport
/// lets the same request handling run on an active download connection.
pub(crate) struct BtUploadState {
    am_choke_state: bool,
    auto_unchoke: bool,
    peer_interested: bool,
    uploaded_bytes: u64,
    upload_limiter: Option<RateLimiter>,
    global_upload_limiter: Option<RateLimiter>,
    message_validator: Option<BtMessageValidator>,
    upload_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

#[async_trait]
pub(crate) trait BtUploadTransport {
    async fn send_upload_message(&mut self, message: &BtMessage)
    -> std::result::Result<(), String>;
    async fn send_upload_choke(&mut self) -> std::result::Result<(), String>;
    async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String>;
}

#[async_trait]
impl BtUploadTransport for PeerConnection {
    async fn send_upload_message(
        &mut self,
        message: &BtMessage,
    ) -> std::result::Result<(), String> {
        self.send_message(message).await
    }

    async fn send_upload_choke(&mut self) -> std::result::Result<(), String> {
        self.send_choke().await
    }

    async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String> {
        self.send_unchoke().await
    }
}

impl BtUploadState {
    pub(crate) fn new(config: &BtSeedingConfig) -> Self {
        let upload_limiter = config
            .max_upload_bytes_per_sec
            .filter(|&rate| rate > 0)
            .map(|rate| RateLimiter::new(&RateLimiterConfig::new(None, Some(rate))));

        Self {
            am_choke_state: false,
            auto_unchoke: true,
            peer_interested: false,
            uploaded_bytes: 0,
            upload_limiter,
            global_upload_limiter: config.global_limiter.clone(),
            message_validator: None,
            upload_counter: None,
        }
    }

    pub(crate) fn set_upload_counter(
        &mut self,
        counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.upload_counter = Some(counter);
    }

    pub(crate) fn set_auto_unchoke(&mut self, enabled: bool) {
        self.auto_unchoke = enabled;
        if !enabled {
            self.am_choke_state = true;
        }
    }

    pub(crate) fn configure_message_validator(&mut self, num_pieces: u32, piece_length: u32) {
        self.message_validator = Some(BtMessageValidator::new(num_pieces, piece_length));
    }

    pub(crate) async fn send_piece_availability<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
        provider: &dyn PieceDataProvider,
    ) -> Result<()> {
        let num_pieces = provider.num_pieces();
        let message = if num_pieces == 0 {
            BtMessage::HaveNone
        } else {
            let mut bitfield = vec![0u8; (num_pieces as usize).div_ceil(8)];
            for piece_index in 0..num_pieces {
                if provider.has_piece(piece_index) {
                    bitfield[piece_index as usize / 8] |= 1 << (7 - piece_index % 8);
                }
            }
            BtMessage::Bitfield { data: bitfield }
        };
        transport
            .send_upload_message(&message)
            .await
            .map_err(|error| {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::TemporaryNetworkFailure { message: error },
                )
            })
    }

    pub(crate) async fn handle_message<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
        message: BtMessage,
        provider: &dyn PieceDataProvider,
    ) -> Result<u64> {
        let round_uploaded = self.uploaded_bytes;
        if let Some(validator) = &self.message_validator {
            validator.validate(&message).map_err(|error| {
                crate::error::Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "invalid BitTorrent upload message: {error}"
                )))
            })?;
        }

        match message {
            BtMessage::Request { request } => {
                if !self.am_choke_state && self.peer_interested {
                    debug!(
                        "Upload request: piece={}, offset={}, len={}",
                        request.index, request.begin, request.length
                    );
                    if let Some(piece_data) = provider.has_piece(request.index).then(|| {
                        provider.get_piece_data(request.index, request.begin, request.length)
                    }) {
                        if let Some(piece_data) = piece_data.await {
                            let data_len = piece_data.len() as u64;
                            if data_len != request.length as u64 {
                                warn!(
                                    "Piece provider returned {} bytes for a {}-byte request (piece={}, offset={})",
                                    data_len, request.length, request.index, request.begin
                                );
                            } else {
                                if let Some(ref limiter) = self.upload_limiter {
                                    limiter.acquire_upload(data_len).await;
                                }
                                if let Some(ref limiter) = self.global_upload_limiter
                                    && limiter.is_upload_limited()
                                {
                                    limiter.acquire_upload(data_len).await;
                                }
                                transport
                                    .send_upload_message(&BtMessage::Piece {
                                        index: request.index,
                                        begin: request.begin,
                                        data: piece_data.into(),
                                    })
                                    .await
                                    .map_err(|error| {
                                        crate::error::Aria2Error::Recoverable(
                                            crate::error::RecoverableError::TemporaryNetworkFailure {
                                                message: error,
                                            },
                                        )
                                    })?;
                                self.uploaded_bytes += data_len;
                                if let Some(counter) = &self.upload_counter {
                                    counter
                                        .fetch_add(data_len, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
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
                if self.auto_unchoke && !self.am_choke_state {
                    transport.send_upload_unchoke().await.ok();
                }
            }
            BtMessage::NotInterested => self.peer_interested = false,
            BtMessage::Choke => debug!("Peer choked us"),
            BtMessage::Unchoke => debug!("Peer unchoked us"),
            BtMessage::Have { piece_index } => debug!("Peer has piece {}", piece_index),
            BtMessage::Cancel { request } => debug!(
                "Peer cancelled request for piece {} offset {}",
                request.index, request.begin
            ),
            BtMessage::Piece { .. } => debug!("Unexpected Piece from peer during upload"),
            BtMessage::Bitfield { .. }
            | BtMessage::KeepAlive
            | BtMessage::Port { .. }
            | BtMessage::AllowedFast { .. }
            | BtMessage::Reject { .. }
            | BtMessage::Suggest { .. }
            | BtMessage::HaveAll
            | BtMessage::HaveNone
            | BtMessage::Extended { .. } => {}
        }

        Ok(self.uploaded_bytes - round_uploaded)
    }

    pub(crate) async fn unchoke_peer<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
    ) -> Result<()> {
        if self.am_choke_state {
            transport.send_upload_unchoke().await.map_err(|error| {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::TemporaryNetworkFailure { message: error },
                )
            })?;
            self.am_choke_state = false;
        }
        Ok(())
    }

    pub(crate) async fn choke_peer<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
    ) -> Result<()> {
        if !self.am_choke_state {
            transport.send_upload_choke().await.map_err(|error| {
                crate::error::Aria2Error::Recoverable(
                    crate::error::RecoverableError::TemporaryNetworkFailure { message: error },
                )
            })?;
            self.am_choke_state = true;
        }
        Ok(())
    }

    pub(crate) fn is_peer_choked(&self) -> bool {
        self.am_choke_state
    }

    pub(crate) fn is_peer_interested(&self) -> bool {
        self.peer_interested
    }

    pub(crate) fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }
}

pub struct BtUploadSession {
    conn: PeerConnection,
    state: BtUploadState,
    pub(crate) is_dead: bool,
}

impl BtUploadSession {
    pub fn new(conn: PeerConnection, config: &BtSeedingConfig) -> Self {
        Self {
            conn,
            state: BtUploadState::new(config),
            is_dead: false,
        }
    }

    pub fn configure_message_validator(&mut self, num_pieces: u32, piece_length: u32) {
        self.state
            .configure_message_validator(num_pieces, piece_length);
    }

    /// Announce the pieces currently available from this upload peer.
    ///
    /// A leecher must receive availability before it can decide whether to
    /// become interested. This is especially important for peers admitted by
    /// the process-level incoming listener, where no download-side setup
    /// message has been sent yet.
    pub async fn send_piece_availability(
        &mut self,
        provider: &dyn PieceDataProvider,
    ) -> Result<()> {
        self.state
            .send_piece_availability(&mut self.conn, provider)
            .await
    }

    pub async fn handle_incoming_messages(
        &mut self,
        provider: &dyn PieceDataProvider,
    ) -> Result<u64> {
        if self.is_dead {
            return Ok(0);
        }

        let round_uploaded = self.state.uploaded_bytes();
        match self.conn.read_message().await {
            Ok(Some(msg)) => {
                if let Err(error) = self
                    .state
                    .handle_message(&mut self.conn, msg, provider)
                    .await
                {
                    warn!("Invalid BitTorrent upload message: {}", error);
                    self.is_dead = true;
                    return Ok(0);
                }
                Ok(self.state.uploaded_bytes() - round_uploaded)
            }
            Ok(None) => {
                debug!("EOF from peer, marking session as dead");
                self.is_dead = true;
                Ok(0)
            }
            Err(e) => {
                warn!("Read error from upload peer: {}, marking dead", e);
                self.is_dead = true;
                Ok(0)
            }
        }
    }

    pub async fn unchoke_peer(&mut self) -> Result<()> {
        self.state.unchoke_peer(&mut self.conn).await
    }

    pub async fn choke_peer(&mut self) -> Result<()> {
        self.state.choke_peer(&mut self.conn).await
    }

    pub fn is_peer_choked(&self) -> bool {
        self.state.is_peer_choked()
    }

    pub fn is_peer_interested(&self) -> bool {
        self.state.is_peer_interested()
    }

    pub fn is_dead(&self) -> bool {
        self.is_dead
    }

    pub fn endpoint(&self) -> Option<(String, u16)> {
        self.conn
            .remote_addr()
            .map(|addr| (addr.ip().to_string(), addr.port()))
    }

    pub fn remote_endpoint(&self) -> Option<std::net::SocketAddr> {
        self.conn.remote_addr()
    }

    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        self.conn.remote_peer_id().copied()
    }

    pub fn uploaded_bytes(&self) -> u64 {
        self.state.uploaded_bytes()
    }

    pub fn connection_mut(&mut self) -> Option<&mut PeerConnection> {
        Some(&mut self.conn)
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

#[async_trait]
impl PieceDataProvider for InMemoryPieceProvider {
    async fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
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

    fn piece_length(&self) -> u32 {
        self.piece_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestUploadTransport {
        sent: Vec<BtMessage>,
    }

    #[async_trait]
    impl BtUploadTransport for TestUploadTransport {
        async fn send_upload_message(
            &mut self,
            message: &BtMessage,
        ) -> std::result::Result<(), String> {
            self.sent.push(message.clone());
            Ok(())
        }

        async fn send_upload_choke(&mut self) -> std::result::Result<(), String> {
            self.sent.push(BtMessage::Choke);
            Ok(())
        }

        async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String> {
            self.sent.push(BtMessage::Unchoke);
            Ok(())
        }
    }

    #[test]
    fn test_seeding_config_default() {
        let cfg = BtSeedingConfig::default();
        assert!(cfg.max_upload_bytes_per_sec.is_none());
        assert_eq!(cfg.max_peers_to_unchoke, 4);
        assert_eq!(cfg.optimistic_unchoke_interval_secs, 30);
    }

    #[tokio::test]
    async fn test_in_memory_provider_creation() {
        let provider = InMemoryPieceProvider::new(16384, 10);
        assert_eq!(provider.num_pieces(), 10);
        assert_eq!(provider.piece_length(), 16384);
        assert!(!provider.has_piece(0));
        assert!(provider.get_piece_data(0, 0, 100).await.is_none());
    }

    #[tokio::test]
    async fn test_in_memory_provider_set_and_get() {
        let mut provider = InMemoryPieceProvider::new(256, 4);
        assert_eq!(provider.piece_length(), 256);
        provider.set_piece_data(0, vec![0xAB; 256]);
        provider.set_piece_data(2, vec![0xCD; 128]);

        assert!(provider.has_piece(0));
        assert!(!provider.has_piece(1));
        assert!(provider.has_piece(2));

        let data = provider.get_piece_data(0, 10, 50).await.unwrap();
        assert_eq!(data.len(), 50);
        assert!(data.iter().all(|&b| b == 0xAB));

        let partial = provider.get_piece_data(2, 100, 28).await.unwrap();
        assert_eq!(partial.len(), 28);
        assert!(partial.iter().all(|&b| b == 0xCD));
    }

    #[tokio::test]
    async fn test_in_memory_provider_set_all_from_pattern() {
        let mut provider = InMemoryPieceProvider::new(100, 5);
        provider.set_all_from_pattern(|piece_idx, byte_idx| {
            ((piece_idx * 37 + byte_idx * 13) % 256) as u8
        });

        for i in 0..5u32 {
            assert!(provider.has_piece(i));
            let data = provider.get_piece_data(i, 0, 100).await.unwrap();
            for (j, &byte) in data.iter().enumerate() {
                assert_eq!(byte, ((i * 37 + j as u32 * 13) % 256) as u8);
            }
        }
    }

    #[tokio::test]
    async fn test_in_memory_provider_offset_beyond_piece() {
        let mut provider = InMemoryPieceProvider::new(50, 2);
        provider.set_piece_data(0, vec![0x42; 50]);

        assert!(provider.get_piece_data(0, 40, 20).await.is_some());
        assert!(provider.get_piece_data(0, 60, 10).await.is_none());
        assert!(provider.get_piece_data(99, 0, 10).await.is_none());
    }

    #[tokio::test]
    async fn test_in_memory_provider_last_piece_smaller() {
        let total_size = 260u32;
        let piece_len = 100u32;
        let num_pieces = total_size.div_ceil(piece_len);
        let mut provider = InMemoryPieceProvider::new(piece_len, num_pieces);

        provider.set_all_from_pattern(|_, idx| idx as u8);

        assert!(provider.has_piece(0));
        assert!(provider.has_piece(1));
        assert!(provider.has_piece(2));

        let last_piece = provider.get_piece_data(2, 0, 60).await.unwrap();
        assert_eq!(last_piece.len(), 60);
    }

    #[tokio::test]
    async fn upload_state_serves_a_verified_piece_while_download_is_active() {
        let mut provider = InMemoryPieceProvider::new(32, 2);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport { sent: Vec::new() };

        state
            .handle_message(&mut transport, BtMessage::Interested, &provider)
            .await
            .unwrap();
        let uploaded = state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                        index: 0,
                        begin: 4,
                        length: 8,
                    },
                },
                &provider,
            )
            .await
            .unwrap();

        assert_eq!(uploaded, 8);
        assert!(matches!(transport.sent.first(), Some(BtMessage::Unchoke)));
        assert!(matches!(
            transport.sent.get(1),
            Some(BtMessage::Piece { index: 0, begin: 4, data }) if data.as_ref() == &[0x5a; 8]
        ));
        assert_eq!(state.uploaded_bytes(), 8);
    }

    #[tokio::test]
    async fn upload_state_policy_keeps_peer_choked_until_rotation_allows_it() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x3c; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        state.set_auto_unchoke(false);
        let mut transport = TestUploadTransport { sent: Vec::new() };

        state
            .handle_message(&mut transport, BtMessage::Interested, &provider)
            .await
            .unwrap();
        let uploaded = state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                        index: 0,
                        begin: 0,
                        length: 8,
                    },
                },
                &provider,
            )
            .await
            .unwrap();

        assert_eq!(uploaded, 0);
        assert!(state.is_peer_choked());
        assert!(transport.sent.is_empty());
    }
}
