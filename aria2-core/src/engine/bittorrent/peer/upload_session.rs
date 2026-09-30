//! Shared upload policy and request handling for torrent-owned peer actors.
//!
//! This module does not own peer transport I/O; the long-lived peer actor is
//! the sole runtime owner of each connection.

use async_trait::async_trait;
use std::collections::VecDeque;
use std::time::Duration;
use tracing::{debug, warn};

use crate::error::Result;
use crate::rate_limiter::RateLimiter;

use crate::engine::bittorrent::peer::message_validation::BtMessageValidator;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::message::types::PieceBlockRequest;

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
    upload_limiter: RateLimiter,
    global_upload_limiter: Option<RateLimiter>,
    message_validator: Option<BtMessageValidator>,
    upload_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    outbound: VecDeque<PendingUploadMessage>,
}

const MAX_PENDING_UPLOAD_MESSAGES: usize = 64;
const UPLOAD_TOKEN_WAIT_SLICE: Duration = Duration::from_millis(25);

enum PendingUploadMessage {
    /// Piece data is loaded only when the queue is flushed, so a matching
    /// inbound Cancel can invalidate this item first.
    Piece {
        request: PieceBlockRequest,
        local_tokens_acquired: bool,
    },
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

impl BtUploadState {
    pub(crate) fn new_with_limiter(config: &BtSeedingConfig, upload_limiter: RateLimiter) -> Self {
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

    #[cfg(test)]
    fn new(config: &BtSeedingConfig) -> Self {
        Self::new_with_limiter(config, RateLimiter::unlimited())
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
        let supports_fast_extension = transport.supports_fast_extension();
        let mut bitfield = vec![0u8; (num_pieces as usize).div_ceil(8)];
        let mut has_any_piece = false;
        let mut has_all_pieces = true;
        for piece_index in 0..num_pieces {
            if provider.has_piece(piece_index) {
                bitfield[piece_index as usize / 8] |= 1 << (7 - piece_index % 8);
                has_any_piece = true;
            } else {
                has_all_pieces = false;
            }
        }

        let message = if supports_fast_extension && has_all_pieces {
            BtMessage::HaveAll
        } else if !has_any_piece {
            if supports_fast_extension {
                BtMessage::HaveNone
            } else {
                return Ok(());
            }
        } else {
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
                        self.outbound.push_back(PendingUploadMessage::Piece {
                            request,
                            local_tokens_acquired: false,
                        });
                    } else if transport.supports_fast_extension() {
                        self.queue_reject_if_fast(transport, &request).await?;
                    } else {
                        return Err(crate::error::Aria2Error::Recoverable(
                            crate::error::RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "peer exceeded the {}-request upload queue limit",
                                    MAX_PENDING_UPLOAD_MESSAGES
                                ),
                            },
                        ));
                    }
                } else {
                    debug!(
                        piece = request.index,
                        choked = self.am_choke_state && !allowed_while_choked,
                        has_piece,
                        "Cannot queue upload response for request"
                    );
                    self.queue_reject_if_fast(transport, &request).await?;
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
                let mut refund_local_tokens = 0u64;
                self.outbound.retain(|message| match message {
                    PendingUploadMessage::Piece {
                        request: pending,
                        local_tokens_acquired,
                    } if pending == &request => {
                        if *local_tokens_acquired {
                            refund_local_tokens =
                                refund_local_tokens.saturating_add(u64::from(pending.length));
                        }
                        false
                    }
                    _ => true,
                });
                if refund_local_tokens > 0 {
                    self.upload_limiter.refund_upload(refund_local_tokens);
                }
                let canceled = before != self.outbound.len();
                debug!(
                    piece = request.index,
                    offset = request.begin,
                    canceled,
                    "Peer cancelled queued upload response"
                );
                if canceled {
                    self.queue_reject_if_fast(transport, &request).await?;
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
            .filter(|message| matches!(message, PendingUploadMessage::Piece { .. }))
            .count()
    }

    pub(crate) fn pending_flush_delay(&self) -> Option<Duration> {
        match self.outbound.front()? {
            PendingUploadMessage::Message(_) => Some(Duration::ZERO),
            PendingUploadMessage::Piece {
                request,
                local_tokens_acquired: false,
            } => {
                let bytes = u64::from(request.length);
                let local_wait = self.upload_limiter.upload_wait(bytes);
                let global_wait = self
                    .global_upload_limiter
                    .as_ref()
                    .filter(|limiter| limiter.is_upload_limited())
                    .map_or(Duration::ZERO, |limiter| limiter.upload_wait(bytes));
                Some(local_wait.max(global_wait))
            }
            PendingUploadMessage::Piece {
                request,
                local_tokens_acquired: true,
            } => Some(
                self.global_upload_limiter
                    .as_ref()
                    .filter(|limiter| limiter.is_upload_limited())
                    .map_or(Duration::ZERO, |limiter| {
                        limiter.upload_wait(u64::from(request.length))
                    }),
            ),
        }
    }

    pub(crate) fn rate_change_receivers(
        &self,
    ) -> (
        tokio::sync::watch::Receiver<u64>,
        Option<tokio::sync::watch::Receiver<u64>>,
    ) {
        (
            self.upload_limiter.subscribe_upload_rate_changes(),
            self.global_upload_limiter
                .as_ref()
                .map(RateLimiter::subscribe_upload_rate_changes),
        )
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
                PendingUploadMessage::Piece {
                    request,
                    mut local_tokens_acquired,
                } => {
                    let requested_len = u64::from(request.length);
                    if !local_tokens_acquired {
                        if tokio::time::timeout(
                            UPLOAD_TOKEN_WAIT_SLICE,
                            self.upload_limiter.acquire_upload(requested_len),
                        )
                        .await
                        .is_err()
                        {
                            self.outbound.push_front(PendingUploadMessage::Piece {
                                request,
                                local_tokens_acquired: false,
                            });
                            return Ok(self.uploaded_bytes - uploaded_before);
                        }
                        local_tokens_acquired = true;
                    }
                    if let Some(limiter) = self.global_upload_limiter.as_ref()
                        && limiter.is_upload_limited()
                        && tokio::time::timeout(
                            UPLOAD_TOKEN_WAIT_SLICE,
                            limiter.acquire_upload(requested_len),
                        )
                        .await
                        .is_err()
                    {
                        self.outbound.push_front(PendingUploadMessage::Piece {
                            request,
                            local_tokens_acquired,
                        });
                        return Ok(self.uploaded_bytes - uploaded_before);
                    }
                    let Some(data) = provider
                        .get_piece_data(request.index, request.begin, request.length)
                        .await
                    else {
                        if local_tokens_acquired {
                            self.upload_limiter.refund_upload(requested_len);
                        }
                        if let Some(limiter) = self.global_upload_limiter.as_ref()
                            && limiter.is_upload_limited()
                        {
                            limiter.refund_upload(requested_len);
                        }
                        warn!(
                            piece = request.index,
                            offset = request.begin,
                            "No data for queued upload request"
                        );
                        self.queue_reject_if_fast(transport, &request).await?;
                        continue;
                    };
                    let data_len = data.len() as u64;
                    if data_len != requested_len {
                        if local_tokens_acquired {
                            self.upload_limiter.refund_upload(requested_len);
                        }
                        if let Some(limiter) = self.global_upload_limiter.as_ref()
                            && limiter.is_upload_limited()
                        {
                            limiter.refund_upload(requested_len);
                        }
                        warn!(
                            "Piece provider returned {} bytes for a {}-byte request (piece={}, offset={})",
                            data_len, request.length, request.index, request.begin
                        );
                        self.queue_reject_if_fast(transport, &request).await?;
                        continue;
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

    async fn queue_reject_if_fast<T: BtUploadTransport>(
        &mut self,
        transport: &mut T,
        request: &PieceBlockRequest,
    ) -> Result<()> {
        if !transport.supports_fast_extension() {
            return Ok(());
        }
        let reject = BtMessage::Reject {
            index: request.index,
            offset: request.begin,
            length: request.length,
        };
        if self.outbound.len() < MAX_PENDING_UPLOAD_MESSAGES {
            self.outbound
                .push_back(PendingUploadMessage::Message(reject));
            return Ok(());
        }
        send_upload_message(transport, &reject).await
    }

    pub(crate) fn discard_pending_messages(&mut self) {
        let mut refund_local_tokens = 0u64;
        for message in self.outbound.drain(..) {
            if let PendingUploadMessage::Piece {
                request,
                local_tokens_acquired: true,
            } = message
            {
                refund_local_tokens = refund_local_tokens.saturating_add(u64::from(request.length));
            }
        }
        if refund_local_tokens > 0 {
            self.upload_limiter.refund_upload(refund_local_tokens);
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

    #[cfg(test)]
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
    async fn startup_availability_matches_fast_extension_capabilities() {
        let mut no_pieces = InMemoryPieceProvider::new(16, 2);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut non_fast_transport = TestUploadTransport::default();
        state
            .send_piece_availability(&mut non_fast_transport, &no_pieces)
            .await
            .unwrap();
        assert!(
            non_fast_transport.sent.is_empty(),
            "without Fast Extension, do not send an empty bitfield"
        );

        let mut fast_transport = TestUploadTransport {
            supports_fast_extension: true,
            ..TestUploadTransport::default()
        };
        state
            .send_piece_availability(&mut fast_transport, &no_pieces)
            .await
            .unwrap();
        assert_eq!(fast_transport.sent, [BtMessage::HaveNone]);

        no_pieces.set_piece_data(0, vec![0x11; 16]);
        non_fast_transport.sent.clear();
        state
            .send_piece_availability(&mut non_fast_transport, &no_pieces)
            .await
            .unwrap();
        assert_eq!(
            non_fast_transport.sent,
            [BtMessage::Bitfield {
                data: vec![0b1000_0000]
            }]
        );

        no_pieces.set_piece_data(1, vec![0x22; 16]);
        fast_transport.sent.clear();
        state
            .send_piece_availability(&mut fast_transport, &no_pieces)
            .await
            .unwrap();
        assert_eq!(fast_transport.sent, [BtMessage::HaveAll]);
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

    #[tokio::test]
    async fn rate_limited_upload_flush_yields_and_preserves_the_request() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let limiter = crate::rate_limiter::RateLimiter::new(
            &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
        );
        let config = BtSeedingConfig {
            global_limiter: Some(limiter.clone()),
            ..BtSeedingConfig::default()
        };
        let mut state = BtUploadState::new(&config);
        let mut transport = TestUploadTransport::default();
        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: PieceBlockRequest::new(0, 0, 8),
                },
                &provider,
            )
            .await
            .unwrap();

        let uploaded = tokio::time::timeout(
            Duration::from_millis(200),
            state.flush_pending_messages(&mut transport, &provider),
        )
        .await
        .expect("rate-limited flush must return control to the peer actor")
        .unwrap();
        assert_eq!(uploaded, 0);
        assert_eq!(state.outstanding_upload_count(), 1);

        limiter.set_upload_rate(None);
        assert_eq!(
            state
                .flush_pending_messages(&mut transport, &provider)
                .await
                .unwrap(),
            8
        );
        assert_eq!(state.outstanding_upload_count(), 0);
    }

    #[tokio::test]
    async fn full_upload_queue_rejects_fast_peer_instead_of_silently_dropping_request() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport {
            supports_fast_extension: true,
            ..TestUploadTransport::default()
        };
        for _ in 0..MAX_PENDING_UPLOAD_MESSAGES {
            state
                .handle_message(
                    &mut transport,
                    BtMessage::Request {
                        request: PieceBlockRequest::new(0, 0, 8),
                    },
                    &provider,
                )
                .await
                .unwrap();
        }

        state
            .handle_message(
                &mut transport,
                BtMessage::Request {
                    request: PieceBlockRequest::new(0, 8, 8),
                },
                &provider,
            )
            .await
            .unwrap();

        assert_eq!(
            state.outstanding_upload_count(),
            MAX_PENDING_UPLOAD_MESSAGES
        );
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
    async fn full_upload_queue_disconnects_non_fast_peer_instead_of_silently_dropping_request() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let mut state = BtUploadState::new(&BtSeedingConfig::default());
        let mut transport = TestUploadTransport::default();
        for _ in 0..MAX_PENDING_UPLOAD_MESSAGES {
            state
                .handle_message(
                    &mut transport,
                    BtMessage::Request {
                        request: PieceBlockRequest::new(0, 0, 8),
                    },
                    &provider,
                )
                .await
                .unwrap();
        }

        assert!(
            state
                .handle_message(
                    &mut transport,
                    BtMessage::Request {
                        request: PieceBlockRequest::new(0, 8, 8),
                    },
                    &provider,
                )
                .await
                .is_err()
        );
        assert_eq!(
            state.outstanding_upload_count(),
            MAX_PENDING_UPLOAD_MESSAGES
        );
    }

    #[tokio::test]
    async fn cancel_is_processed_after_rate_limited_flush_yields() {
        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x5a; 32]);
        let limiter = crate::rate_limiter::RateLimiter::new(
            &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
        );
        let config = BtSeedingConfig {
            global_limiter: Some(limiter),
            ..BtSeedingConfig::default()
        };
        let mut state = BtUploadState::new(&config);
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
            .flush_pending_messages(&mut transport, &provider)
            .await
            .unwrap();

        state
            .handle_message(&mut transport, BtMessage::Cancel { request }, &provider)
            .await
            .unwrap();
        assert_eq!(state.outstanding_upload_count(), 0);
        assert!(transport.sent.is_empty());
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
            Some(BtMessage::Piece { index: 0, begin: 4, data }) if data.as_ref() == [0x5a; 8]
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
            [BtMessage::Piece { index: 0, begin: 0, data }] if data.as_ref() == [0x7b; 8]
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
            }] if data.as_ref() == [0x6d; 8]
        ));
    }
}
