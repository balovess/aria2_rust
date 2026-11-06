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

mod in_memory_provider;
pub use in_memory_provider::InMemoryPieceProvider;

/// Supplies verified torrent pieces to peer upload sessions.
///
/// Implementations must report a piece available only after its integrity has
/// been verified. Reads return the exact requested range, or `None` when the
/// piece/range is unavailable; short reads are treated as unavailable data.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait PieceDataProvider: Send + Sync {
    /// Read an exact byte range from a verified piece.
    async fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>>;

    /// Whether the complete piece is locally available and verified.
    fn has_piece(&self, piece_index: u32) -> bool;

    /// Number of pieces described by this provider.
    fn num_pieces(&self) -> u32;

    /// Nominal size of each piece; the final piece may be shorter.
    fn piece_length(&self) -> u32;
}

/// Upload rate and unchoke policy shared by peer actors and seed managers.
#[derive(Clone)]
pub struct BtSeedingConfig {
    /// Per-torrent upload limit in bytes per second. `None` is unlimited.
    pub max_upload_bytes_per_sec: Option<u64>,
    /// Process-wide limiter shared by all download/seeding commands.
    pub global_limiter: Option<RateLimiter>,
    /// Maximum number of interested peers to unchoke at once.
    pub max_peers_to_unchoke: usize,
    /// Period between optimistic-unchoke rotations, in seconds.
    pub optimistic_unchoke_interval_secs: u64,
}

impl Default for BtSeedingConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes_per_sec: None,
            global_limiter: None,
            max_peers_to_unchoke: crate::constants::BT_DEFAULT_MAX_UPLOAD_SLOTS,
            optimistic_unchoke_interval_secs: crate::constants::BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS,
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

#[allow(clippy::double_must_use)]
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
                if self.auto_unchoke && !self.am_choke_state {
                    transport.send_upload_unchoke().await.ok();
                }
            }
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
            | BtMessage::NotInterested
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

#[cfg(test)]
#[path = "upload_session/tests.rs"]
mod tests;
