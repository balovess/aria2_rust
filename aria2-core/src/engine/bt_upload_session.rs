use async_trait::async_trait;
use std::collections::VecDeque;
use tracing::{debug, warn};

use crate::error::Result;
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::RateLimiterConfig;

use crate::engine::bt_message_validation::BtMessageValidator;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::message::types::PieceBlockRequest;
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
    outbound: VecDeque<PendingUploadMessage>,
}

const MAX_PENDING_UPLOAD_MESSAGES: usize = 64;

enum PendingUploadMessage {
    /// Piece data is loaded only when the queue is flushed, so a matching
    /// inbound Cancel can invalidate this item first.
    Piece(PieceBlockRequest),
    Message(BtMessage),
}

#[async_trait]
pub(crate) trait BtUploadTransport {
    fn supports_fast_extension(&self) -> bool {
        false
    }
    fn am_allowed_fast(&self, _piece_index: u32) -> bool {
        false
    }
    async fn send_upload_message(&mut self, message: &BtMessage)
    -> std::result::Result<(), String>;
    async fn send_upload_choke(&mut self) -> std::result::Result<(), String>;
    async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String>;
}

#[async_trait]
impl BtUploadTransport for PeerConnection {
    fn supports_fast_extension(&self) -> bool {
        self.remote_supports_fast_extension()
    }

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
            outbound: VecDeque::new(),
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
        if let Some(validator) = &self.message_validator {
            validator.validate(&message).map_err(|error| {
                crate::error::Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "invalid BitTorrent upload message: {error}"
                )))
            })?;
        }

        match message {
            BtMessage::Request { request } => {
                let allowed_while_choked = transport.am_allowed_fast(request.index);
                let has_piece = provider.has_piece(request.index);
                if (!self.am_choke_state || allowed_while_choked) && has_piece {
                    debug!(
                        "Upload request: piece={}, offset={}, len={}",
                        request.index, request.begin, request.length
                    );
                    if self.outbound.len() < MAX_PENDING_UPLOAD_MESSAGES {
                        self.outbound
                            .push_back(PendingUploadMessage::Piece(request));
                    }
                } else {
                    debug!(
                        piece = request.index,
                        choked = self.am_choke_state && !allowed_while_choked,
                        has_piece,
                        "Cannot queue upload response for request"
                    );
                    self.queue_reject_if_fast(transport, &request);
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
            BtMessage::Cancel { request } => {
                let before = self.outbound.len();
                self.outbound.retain(|message| {
                    !matches!(message, PendingUploadMessage::Piece(pending) if pending == &request)
                });
                let canceled = before != self.outbound.len();
                debug!(
                    piece = request.index,
                    offset = request.begin,
                    canceled,
                    "Peer cancelled queued upload response"
                );
                if canceled {
                    self.queue_reject_if_fast(transport, &request);
                }
            }
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

        Ok(0)
    }

    pub(crate) fn has_pending_messages(&self) -> bool {
        !self.outbound.is_empty()
    }

    pub(crate) fn outstanding_upload_count(&self) -> usize {
        self.outbound
            .iter()
            .filter(|message| matches!(message, PendingUploadMessage::Piece(_)))
            .count()
    }

    pub(crate) async fn flush_pending_messages<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
        provider: &dyn PieceDataProvider,
    ) -> Result<u64> {
        let uploaded_before = self.uploaded_bytes;
        while let Some(message) = self.outbound.pop_front() {
            match message {
                PendingUploadMessage::Message(message) => {
                    send_upload_message(transport, &message).await?;
                }
                PendingUploadMessage::Piece(request) => {
                    let Some(data) = provider
                        .get_piece_data(request.index, request.begin, request.length)
                        .await
                    else {
                        warn!(
                            piece = request.index,
                            offset = request.begin,
                            "No data for queued upload request"
                        );
                        self.queue_reject_if_fast(transport, &request);
                        continue;
                    };
                    let data_len = data.len() as u64;
                    if data_len != request.length as u64 {
                        warn!(
                            "Piece provider returned {} bytes for a {}-byte request (piece={}, offset={})",
                            data_len, request.length, request.index, request.begin
                        );
                        self.queue_reject_if_fast(transport, &request);
                        continue;
                    }
                    if let Some(ref limiter) = self.upload_limiter {
                        limiter.acquire_upload(data_len).await;
                    }
                    if let Some(ref limiter) = self.global_upload_limiter
                        && limiter.is_upload_limited()
                    {
                        limiter.acquire_upload(data_len).await;
                    }
                    let message = BtMessage::Piece {
                        index: request.index,
                        begin: request.begin,
                        data: data.into(),
                    };
                    send_upload_message(transport, &message).await?;
                    self.uploaded_bytes += data_len;
                    if let Some(counter) = &self.upload_counter {
                        counter.fetch_add(data_len, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
        Ok(self.uploaded_bytes - uploaded_before)
    }

    fn queue_reject_if_fast<T: BtUploadTransport>(
        &mut self,
        transport: &T,
        request: &PieceBlockRequest,
    ) {
        if transport.supports_fast_extension() && self.outbound.len() < MAX_PENDING_UPLOAD_MESSAGES
        {
            self.outbound
                .push_back(PendingUploadMessage::Message(BtMessage::Reject {
                    index: request.index,
                    offset: request.begin,
                    length: request.length,
                }));
        }
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

async fn send_upload_message<T: BtUploadTransport>(
    transport: &mut T,
    message: &BtMessage,
) -> Result<()> {
    transport
        .send_upload_message(message)
        .await
        .map_err(|message| {
            crate::error::Aria2Error::Recoverable(
                crate::error::RecoverableError::TemporaryNetworkFailure { message },
            )
        })?;
    Ok(())
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

        let uploaded_before = self.state.uploaded_bytes();
        let mut flush_deadline: Option<tokio::time::Instant> = None;
        loop {
            let incoming = if let Some(deadline) = flush_deadline {
                tokio::select! {
                    biased;
                    message = self.conn.read_message() => message,
                    _ = tokio::time::sleep_until(deadline) => {
                        if let Err(error) = self.state.flush_pending_messages(&mut self.conn, provider).await {
                            warn!("Failed to flush queued BitTorrent upload responses: {}", error);
                            self.is_dead = true;
                            return Ok(0);
                        }
                        return Ok(self.state.uploaded_bytes() - uploaded_before);
                    }
                }
            } else {
                self.conn.read_message().await
            };

            match incoming {
                Ok(Some(message)) => {
                    if let Err(error) = self
                        .state
                        .handle_message(&mut self.conn, message, provider)
                        .await
                    {
                        warn!("Invalid BitTorrent upload message: {}", error);
                        self.is_dead = true;
                        return Ok(0);
                    }
                    if !self.state.has_pending_messages() {
                        return Ok(self.state.uploaded_bytes() - uploaded_before);
                    }
                    flush_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + std::time::Duration::from_millis(2)
                    });
                }
                Ok(None) => {
                    debug!("EOF from peer, marking session as dead");
                    self.is_dead = true;
                    return Ok(0);
                }
                Err(error) => {
                    warn!("Read error from upload peer: {}, marking dead", error);
                    self.is_dead = true;
                    return Ok(0);
                }
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

    #[derive(Default)]
    struct TestUploadTransport {
        sent: Vec<BtMessage>,
        supports_fast_extension: bool,
        am_allowed_fast: std::collections::HashSet<u32>,
    }

    #[async_trait]
    impl BtUploadTransport for TestUploadTransport {
        fn supports_fast_extension(&self) -> bool {
            self.supports_fast_extension
        }

        fn am_allowed_fast(&self, piece_index: u32) -> bool {
            self.am_allowed_fast.contains(&piece_index)
        }

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

    #[tokio::test]
    async fn outstanding_upload_count_tracks_queued_piece_responses() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: 0,
            begin: 4,
            length: 8,
        };
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport::default();

        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: request.clone(),
                },
                &provider,
            )
            .await
            .unwrap();
        assert_eq!(state.outstanding_upload_count(), 1);

        state
            .handle_message(
                &mut transport,
                BtMessage::Cancel {
                    request: request.clone(),
                },
                &provider,
            )
            .await
            .unwrap();
        assert_eq!(state.outstanding_upload_count(), 0);

        state
            .handle_message(&mut transport, BtMessage::Request { request }, &provider)
            .await
            .unwrap();
        assert_eq!(state.outstanding_upload_count(), 1);
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();
        assert_eq!(state.outstanding_upload_count(), 0);
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
        let mut transport = TestUploadTransport::default();

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
        let uploaded = uploaded
            + state
                .flush_pending_messages(&mut transport, &provider)
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
        let mut transport = TestUploadTransport::default();

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

    #[tokio::test]
    async fn choked_peer_can_request_piece_granted_by_allowed_fast() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x7b; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        state.set_auto_unchoke(false);
        let mut transport = TestUploadTransport {
            am_allowed_fast: [0].into_iter().collect(),
            ..Default::default()
        };

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
        let uploaded = uploaded
            + state
                .flush_pending_messages(&mut transport, &provider)
                .await
                .unwrap();

        assert_eq!(uploaded, 8);
        assert!(matches!(
            transport.sent.as_slice(),
            [BtMessage::Piece { index: 0, begin: 0, data }] if data.as_ref() == &[0x7b; 8]
        ));
    }

    #[tokio::test]
    async fn choked_fast_peer_receives_reject_for_unallowed_piece_request() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x7b; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        state.set_auto_unchoke(false);
        let mut transport = TestUploadTransport {
            supports_fast_extension: true,
            ..Default::default()
        };

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
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

        assert_eq!(uploaded, 0);
        assert!(matches!(
            transport.sent.as_slice(),
            [BtMessage::Reject {
                index: 0,
                offset: 4,
                length: 8
            }]
        ));
    }

    #[tokio::test]
    async fn cancel_removes_queued_piece_and_queues_fast_reject() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x6d; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport {
            supports_fast_extension: true,
            ..Default::default()
        };
        let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: 0,
            begin: 8,
            length: 8,
        };

        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: request.clone(),
                },
                &provider,
            )
            .await
            .unwrap();
        state
            .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
            .await
            .unwrap();

        assert_eq!(transport.sent.len(), 0);
        assert!(state.has_pending_messages());
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();
        assert!(matches!(
            transport.sent.as_slice(),
            [BtMessage::Reject {
                index: 0,
                offset: 8,
                length: 8
            }]
        ));
    }

    #[tokio::test]
    async fn cancel_does_not_reject_when_fast_extension_is_unavailable() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x6d; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport::default();
        let request = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: 0,
            begin: 0,
            length: 8,
        };

        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: request.clone(),
                },
                &provider,
            )
            .await
            .unwrap();
        state
            .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
            .await
            .unwrap();
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

        assert!(transport.sent.is_empty());
    }

    #[tokio::test]
    async fn cancel_only_removes_an_exact_queued_piece_request() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x6d; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport::default();
        let request = PieceBlockRequest::new(0, 0, 8);

        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: request.clone(),
                },
                &provider,
            )
            .await
            .unwrap();
        state
            .handle_message(
                &mut transport,
                BtMessage::Cancel {
                    request: PieceBlockRequest::new(0, 8, 8),
                },
                &provider,
            )
            .await
            .unwrap();
        state
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

        assert!(matches!(
            transport.sent.as_slice(),
            [BtMessage::Piece {
                index: 0,
                begin: 0,
                data
            }] if data.as_ref() == &[0x6d; 8]
        ));
    }

    #[tokio::test]
    async fn upload_session_consumes_cancel_before_flushing_piece_response() {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sender = tokio::net::TcpStream::connect(address).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let peer = PeerConnection::from_stream_with_peer(server, [0; 20], false, true);
        let mut session = BtUploadSession::new(peer, &BtSeedingConfig::default());
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x4a; 32]);
        let request = PieceBlockRequest::new(0, 8, 8);
        let mut input = serialize(&BtMessage::Request {
            request: request.clone(),
        });
        input.extend_from_slice(&serialize(&BtMessage::Cancel { request }));
        let mut sender = sender;
        sender.write_all(&input).await.unwrap();
        let mut receiver = PeerConnection::from_stream_with_peer(sender, [1; 20], false, false);

        let uploaded = session.handle_incoming_messages(&provider).await.unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            receiver.read_message(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        assert_eq!(uploaded, 0);
        assert_eq!(
            response,
            BtMessage::Reject {
                index: 0,
                offset: 8,
                length: 8,
            }
        );

        let request = PieceBlockRequest::new(0, 16, 8);
        receiver
            .send_message(&BtMessage::Request { request })
            .await
            .unwrap();
        let uploaded = session.handle_incoming_messages(&provider).await.unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            receiver.read_message(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(uploaded, 8);
        assert!(matches!(
            response,
            BtMessage::Piece { index: 0, begin: 16, ref data }
                if data.as_ref() == &[0x4a; 8]
        ));
    }
}
