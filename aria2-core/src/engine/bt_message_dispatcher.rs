//! BT Message Dispatcher - Outgoing message queue and request slot management
//!
//! This module implements the C++ `DefaultBtMessageDispatcher` architecture,
//! providing two core data structures:
//!
//! 1. **Outgoing Message Queue** (`messageQueue_` in C++) — FIFO queue of
//!    BT messages to be sent to the peer, with upload-speed-aware deferred
//!    sending.
//!
//! 2. **Request Slot Tracker** (`requestSlots_` in C++) — Tracks outstanding
//!    download requests (Request messages we sent to the peer), with timeout
//!    detection, snubbing, and event-driven cancellation.
//!
//! # C++ Architecture Reference
//!
//! - `src/DefaultBtMessageDispatcher.h/.cc` — Message queue + request slots
//! - `src/RequestSlot.h` — Individual request slot with timeout tracking
//! - `src/BtMessage.h` — Message base class with event handlers
//!
//! # Key Differences from C++
//!
//! - C++ uses virtual dispatch (`BtMessage::onCancelSendingPieceEvent()`,
//!   etc.) for event-driven message invalidation. Rust uses a simpler
//!   approach: `QueuedMessage` carries an `invalidated` flag that can be
//!   set by event handlers.
//! - C++ `RequestSlot` caches a `shared_ptr<Piece>` for performance.
//!   Rust `RequestSlot` tracks the piece index + block index instead,
//!   since piece lookup is cheap in our architecture.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

// ======================================================================
// Constants (matching C++ aria2)
// ======================================================================

/// Default request timeout in seconds (matching C++ PREF_BT_REQUEST_TIMEOUT).
/// If a peer doesn't respond to a Request within this time, they are snubbed.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Maximum number of I/O vector entries for a single writev call.
/// Messages are deferred if the buffer is at this capacity.
/// (Matching C++ A2_IOV_MAX, typically 16 on Linux.)
/// NOTE: Used for buffer capacity gating in future send_pending_data() integration.
#[allow(dead_code)]
const A2_IOV_MAX: usize = 16;

// ======================================================================
// QueuedMessage — a message waiting to be sent
// ======================================================================

/// A BT message in the outgoing queue.
///
/// Mirrors the C++ `messageQueue_` entry. Each message can be invalidated
/// by event handlers (e.g., `onCancelSendingPieceEvent`), in which case
/// `send()` becomes a no-op.
#[derive(Debug)]
pub struct QueuedMessage {
    /// The serialized message bytes.
    pub data: Vec<u8>,
    /// Whether this message carries piece data (upload).
    /// Upload messages are subject to speed limiting.
    pub is_upload: bool,
    /// Whether this message has been invalidated and should not be sent.
    pub invalidated: bool,
    /// Piece index, begin, length for Piece/Request messages.
    /// Used for event-driven invalidation (cancel, choke, abort).
    pub piece_key: Option<PieceKey>,
}

/// Key identifying a specific piece block for event matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceKey {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

impl QueuedMessage {
    /// Create a new non-upload queued message.
    pub fn control_message(data: Vec<u8>) -> Self {
        Self {
            data,
            is_upload: false,
            invalidated: false,
            piece_key: None,
        }
    }

    /// Create a new upload queued message (Piece message carrying data).
    pub fn upload_message(data: Vec<u8>, piece_key: PieceKey) -> Self {
        Self {
            data,
            is_upload: true,
            invalidated: false,
            piece_key: Some(piece_key),
        }
    }

    /// Create a new request queued message (Request message we're sending out).
    pub fn request_message(data: Vec<u8>, piece_key: PieceKey) -> Self {
        Self {
            data,
            is_upload: false,
            invalidated: false,
            piece_key: Some(piece_key),
        }
    }

    /// Invalidate this message (it will be skipped during sending).
    /// Mirrors C++ `BtMessage::invalidate()` / event-driven invalidation.
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    /// Check if this message matches a specific piece block.
    /// Used for cancel/abort event matching.
    pub fn matches_piece(&self, index: u32, begin: u32, length: u32) -> bool {
        self.piece_key
            .as_ref()
            .map_or(false, |k| k.index == index && k.begin == begin && k.length == length)
    }

    /// Check if this message is for a specific piece index (any block).
    /// Used for abort-outstanding-request event matching.
    pub fn matches_piece_index(&self, index: u32) -> bool {
        self.piece_key
            .as_ref()
            .map_or(false, |k| k.index == index)
    }
}

// ======================================================================
// RequestSlot — an outstanding download request
// ======================================================================

/// Tracks an outstanding download request that we sent to a peer.
///
/// Mirrors C++ `RequestSlot`:
/// - `index_`, `begin_`, `length_` — identify the requested block
/// - `blockIndex_` — which block within the piece (offset / BLOCK_SIZE)
/// - `dispatchedTime_` — when the request was sent, for timeout detection
///
/// When a request times out, the peer is marked as snubbing.
/// When a request is cancelled (block obtained elsewhere), a Cancel
/// message should be sent to the peer.
#[derive(Debug, Clone)]
pub struct RequestSlot {
    /// Piece index.
    pub index: u32,
    /// Byte offset within the piece.
    pub begin: u32,
    /// Block length in bytes.
    pub length: u32,
    /// Block index within the piece (begin / BLOCK_SIZE).
    pub block_index: u32,
    /// When this request was dispatched.
    pub dispatched_time: Instant,
}

impl RequestSlot {
    /// Create a new request slot.
    pub fn new(index: u32, begin: u32, length: u32, block_size: u32) -> Self {
        let block_index = begin / block_size;
        Self {
            index,
            begin,
            length,
            block_index,
            dispatched_time: Instant::now(),
        }
    }

    /// Check if this request has timed out.
    /// Mirrors C++ `RequestSlot::isTimeout(requestTimeout)`.
    pub fn is_timeout(&self, timeout: Duration) -> bool {
        self.dispatched_time.elapsed() > timeout
    }

    /// Check if this slot matches the given (index, begin, length) triple.
    /// Used by `getOutstandingRequest()`.
    pub fn matches(&self, index: u32, begin: u32, length: u32) -> bool {
        self.index == index && self.begin == begin && self.length == length
    }

    /// Check if this slot is for the given piece index and block index.
    /// Used by `isOutstandingRequest()`.
    pub fn matches_block(&self, index: u32, block_index: u32) -> bool {
        self.index == index && self.block_index == block_index
    }
}

impl PartialEq for RequestSlot {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.begin == other.begin && self.length == other.length
    }
}

impl Eq for RequestSlot {}

// ======================================================================
// BtMessageDispatcher — manages outgoing queue + request slots
// ======================================================================

/// BitTorrent message dispatcher, matching C++ `DefaultBtMessageDispatcher`.
///
/// Two main responsibilities:
/// 1. **Message Queue** — FIFO queue of outgoing messages with upload speed
///    limiting and event-driven invalidation.
/// 2. **Request Slots** — Tracks outstanding download requests with timeout
///    detection, choke-based pruning, and abort-based cleanup.
///
/// # Event-Driven Actions
///
/// The dispatcher provides several event handlers that mirror C++ behavior:
///
/// - `do_choked_action()` — Called when we receive a Choke from the peer.
///   Removes outstanding requests that are not in the peer's allowed-fast set.
///
/// - `do_choking_action()` — Called when we send a Choke to the peer.
///   Invalidates all queued Piece upload messages.
///
/// - `do_cancel_sending_piece_action()` — Called when the peer sends a Cancel.
///   Invalidates the matching Piece message in the queue.
///
/// - `do_abort_outstanding_request_action()` — Called when a piece is
///   reassigned. Removes matching request slots and invalidates queued
///   Request messages for that piece.
///
/// - `check_request_slots()` — Periodic maintenance. Removes timed-out
///   request slots (marking the peer as snubbing) and cancels blocks
///   that have already been obtained from another peer.
pub struct BtMessageDispatcher {
    // ── Outgoing message queue ─────────────────────────────────────────
    /// FIFO queue of messages to send (C++ `messageQueue_`).
    message_queue: VecDeque<QueuedMessage>,

    // ── Outstanding request slots ──────────────────────────────────────
    /// Outstanding download requests awaiting response (C++ `requestSlots_`).
    /// `pub(crate)` for test access via `BtPeerMessageHandler`.
    pub(crate) request_slots: VecDeque<RequestSlot>,

    // ── Configuration ──────────────────────────────────────────────────
    /// Request timeout duration (C++ `requestTimeout_`).
    request_timeout: Duration,

    /// Block size for block index calculation.
    block_size: u32,

    // ── Upload speed limiting state ────────────────────────────────────
    /// Whether upload speed limit is currently exceeded.
    /// When true, upload messages are deferred.
    upload_speed_exceeded: bool,

    /// Whether per-group upload speed limit is currently exceeded.
    group_upload_speed_exceeded: bool,
}

impl BtMessageDispatcher {
    /// Create a new dispatcher with default settings.
    pub fn new(block_size: u32) -> Self {
        Self {
            message_queue: VecDeque::new(),
            request_slots: VecDeque::new(),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            block_size,
            upload_speed_exceeded: false,
            group_upload_speed_exceeded: false,
        }
    }

    /// Create a new dispatcher with custom request timeout.
    pub fn with_timeout(block_size: u32, timeout: Duration) -> Self {
        Self {
            message_queue: VecDeque::new(),
            request_slots: VecDeque::new(),
            request_timeout: timeout,
            block_size,
            upload_speed_exceeded: false,
            group_upload_speed_exceeded: false,
        }
    }

    // ── Message Queue Operations ────────────────────────────────────────

    /// Add a message to the outgoing queue.
    /// Mirrors C++ `addMessageToQueue()`.
    pub fn add_message(&mut self, msg: QueuedMessage) {
        debug!(
            "Dispatch: queuing message (upload={}, piece_key={:?}, queue_len={})",
            msg.is_upload,
            msg.piece_key,
            self.message_queue.len()
        );
        self.message_queue.push_back(msg);
    }

    /// Add a control message (non-upload) to the queue.
    pub fn add_control_message(&mut self, data: Vec<u8>) {
        self.add_message(QueuedMessage::control_message(data));
    }

    /// Add a Piece upload message to the queue.
    pub fn add_upload_message(&mut self, data: Vec<u8>, index: u32, begin: u32, length: u32) {
        self.add_message(QueuedMessage::upload_message(
            data,
            PieceKey {
                index,
                begin,
                length,
            },
        ));
    }

    /// Add a Request message to the queue.
    pub fn add_request_message(&mut self, data: Vec<u8>, index: u32, begin: u32, length: u32) {
        self.add_message(QueuedMessage::request_message(
            data,
            PieceKey {
                index,
                begin,
                length,
            },
        ));
    }

    /// Drain messages that are ready to be sent.
    ///
    /// Mirrors C++ `sendMessagesInternal()`. Upload messages are deferred
    /// if speed limits are exceeded. Invalidated messages are skipped.
    ///
    /// Returns a vector of message bytes to write to the socket.
    /// Upload messages that were deferred are re-inserted at the front.
    pub fn drain_sendable_messages(&mut self) -> Vec<Vec<u8>> {
        let mut to_send = Vec::new();
        let mut deferred = VecDeque::new();

        while let Some(msg) = self.message_queue.pop_front() {
            // Skip invalidated messages
            if msg.invalidated {
                debug!("Dispatch: skipping invalidated message");
                continue;
            }

            // Defer upload messages if speed limits are exceeded
            if msg.is_upload && (self.upload_speed_exceeded || self.group_upload_speed_exceeded) {
                debug!("Dispatch: deferring upload message (speed limit)");
                deferred.push_front(msg);
                continue;
            }

            to_send.push(msg.data);
        }

        // Re-insert deferred messages at the front so they retain priority
        for msg in deferred {
            self.message_queue.push_front(msg);
        }

        to_send
    }

    /// Return the number of messages in the queue.
    /// Mirrors C++ `countMessageInQueue()`.
    pub fn count_messages(&self) -> usize {
        self.message_queue.len()
    }

    /// Check if there are any pending messages.
    pub fn has_pending_messages(&self) -> bool {
        !self.message_queue.is_empty()
    }

    /// Count outstanding upload messages in the queue.
    /// Mirrors C++ `countOutstandingUpload()`.
    pub fn count_outstanding_upload(&self) -> usize {
        self.message_queue
            .iter()
            .filter(|m| m.is_upload && !m.invalidated)
            .count()
    }

    // ── Request Slot Operations ─────────────────────────────────────────

    /// Add an outstanding request slot.
    /// Mirrors C++ `addOutstandingRequest()`.
    pub fn add_request_slot(&mut self, index: u32, begin: u32, length: u32) {
        let slot = RequestSlot::new(index, begin, length, self.block_size);
        debug!(
            "Dispatch: added request slot (piece={}, begin={}, len={}, block={})",
            index, begin, length, slot.block_index
        );
        self.request_slots.push_back(slot);
    }

    /// Remove an outstanding request slot by matching (index, begin, length).
    /// Mirrors C++ `removeOutstandingRequest()`.
    /// Returns true if a matching slot was found and removed.
    pub fn remove_request_slot(&mut self, index: u32, begin: u32, length: u32) -> bool {
        if let Some(pos) = self
            .request_slots
            .iter()
            .position(|s| s.matches(index, begin, length))
        {
            self.request_slots.remove(pos);
            true
        } else {
            false
        }
    }

    /// Look up an outstanding request slot by (index, begin, length).
    /// Mirrors C++ `getOutstandingRequest()`.
    pub fn get_request_slot(&self, index: u32, begin: u32, length: u32) -> Option<&RequestSlot> {
        self.request_slots
            .iter()
            .find(|s| s.matches(index, begin, length))
    }

    /// Check if there is an outstanding request for the given piece+block.
    /// Mirrors C++ `isOutstandingRequest()`.
    pub fn is_outstanding_request(&self, index: u32, block_index: u32) -> bool {
        self.request_slots
            .iter()
            .any(|s| s.matches_block(index, block_index))
    }

    /// Return the number of outstanding request slots.
    /// Mirrors C++ `countOutstandingRequest()`.
    pub fn count_request_slots(&self) -> usize {
        self.request_slots.len()
    }

    /// Check if there are any outstanding requests.
    pub fn has_outstanding_requests(&self) -> bool {
        !self.request_slots.is_empty()
    }

    // ── Event-Driven Actions ────────────────────────────────────────────

    /// Handle receiving a Choke message from the peer.
    ///
    /// Mirrors C++ `doChokedAction()`. Removes outstanding request slots
    /// for pieces that are NOT in the peer's allowed-fast set.
    /// The caller provides a closure that checks whether a piece index
    /// is in the allowed-fast set.
    ///
    /// Returns the removed request slots (for the caller to cancel blocks
    /// on the corresponding pieces).
    pub fn do_choked_action<F>(&mut self, is_in_allowed_fast: F) -> Vec<RequestSlot>
    where
        F: Fn(u32) -> bool,
    {
        let mut removed = Vec::new();
        let mut keep = VecDeque::new();

        for slot in self.request_slots.drain(..) {
            if is_in_allowed_fast(slot.index) {
                // Keep this request — it's in the allowed-fast set
                keep.push_back(slot);
            } else {
                debug!(
                    "Dispatch: choked action removing request slot (piece={}, begin={})",
                    slot.index, slot.begin
                );
                removed.push(slot);
            }
        }

        self.request_slots = keep;
        removed
    }

    /// Handle sending a Choke message to the peer.
    ///
    /// Mirrors C++ `doChokingAction()`. Invalidates all queued Piece
    /// upload messages since we are choking the peer and should not
    /// send them data.
    pub fn do_choking_action(&mut self) {
        let mut invalidated_count = 0;
        for msg in self.message_queue.iter_mut() {
            if msg.is_upload && !msg.invalidated {
                msg.invalidate();
                invalidated_count += 1;
            }
        }
        if invalidated_count > 0 {
            debug!(
                "Dispatch: choking action invalidated {} upload messages",
                invalidated_count
            );
        }
    }

    /// Handle receiving a Cancel message from the peer.
    ///
    /// Mirrors C++ `doCancelSendingPieceAction()`. Invalidates any
    /// queued Piece message that matches the specified (index, begin, length).
    pub fn do_cancel_sending_piece_action(&mut self, index: u32, begin: u32, length: u32) {
        let mut found = false;
        for msg in self.message_queue.iter_mut() {
            if msg.matches_piece(index, begin, length) && msg.is_upload && !msg.invalidated {
                msg.invalidate();
                found = true;
                debug!(
                    "Dispatch: cancel action invalidated Piece(piece={}, begin={}, len={})",
                    index, begin, length
                );
            }
        }
        if !found {
            debug!(
                "Dispatch: cancel action found no matching Piece for ({}, {}, {})",
                index, begin, length
            );
        }
    }

    /// Handle aborting all outstanding requests for a specific piece.
    ///
    /// Mirrors C++ `doAbortOutstandingRequestAction()`. Removes matching
    /// request slots and invalidates queued Request messages for that piece.
    /// Called when a piece is reassigned to another peer.
    ///
    /// Returns the removed request slots.
    pub fn do_abort_outstanding_request_action(&mut self, piece_index: u32) -> Vec<RequestSlot> {
        // Remove matching request slots
        let mut removed = Vec::new();
        let mut keep = VecDeque::new();

        for slot in self.request_slots.drain(..) {
            if slot.index == piece_index {
                debug!(
                    "Dispatch: abort action removing request slot (piece={}, begin={})",
                    slot.index, slot.begin
                );
                removed.push(slot);
            } else {
                keep.push_back(slot);
            }
        }
        self.request_slots = keep;

        // Invalidate matching Request messages in the queue
        let mut invalidated_count = 0;
        for msg in self.message_queue.iter_mut() {
            if msg.matches_piece_index(piece_index) && !msg.invalidated {
                msg.invalidate();
                invalidated_count += 1;
            }
        }

        if invalidated_count > 0 || !removed.is_empty() {
            debug!(
                "Dispatch: abort action for piece {} removed {} slots, invalidated {} messages",
                piece_index,
                removed.len(),
                invalidated_count
            );
        }

        removed
    }

    // ── Periodic Maintenance ─────────────────────────────────────────────

    /// Check request slots for timeouts and already-acquired blocks.
    ///
    /// Mirrors C++ `checkRequestSlotAndDoNecessaryThing()`.
    /// This should be called periodically (e.g., once per interaction cycle).
    ///
    /// For each request slot:
    /// 1. If the slot has timed out, mark the peer as snubbing and remove it.
    /// 2. If the block has already been acquired (from another peer),
    ///    remove the slot and return a Cancel message to send.
    ///
    /// The `is_block_acquired` closure should return true if the specified
    /// block has already been downloaded from another peer.
    ///
    /// Returns a `SlotCheckResult` with:
    /// - `timed_out`: true if any slot timed out (caller should mark peer as snubbing)
    /// - `cancelled_blocks`: list of (index, begin, length) that were acquired
    ///   elsewhere and need Cancel messages sent to the peer
    pub fn check_request_slots<F>(&mut self, is_block_acquired: F) -> SlotCheckResult
    where
        F: Fn(u32, u32) -> bool,
    {
        let mut timed_out = false;
        let mut cancelled_blocks = Vec::new();
        let mut keep = VecDeque::new();

        for slot in self.request_slots.drain(..) {
            if slot.is_timeout(self.request_timeout) {
                // Request timed out — peer is snubbing
                warn!(
                    "Dispatch: request slot timed out (piece={}, begin={}, elapsed={:?})",
                    slot.index,
                    slot.begin,
                    slot.dispatched_time.elapsed()
                );
                timed_out = true;
            } else if is_block_acquired(slot.index, slot.block_index) {
                // Block already obtained from another peer — send Cancel
                debug!(
                    "Dispatch: block already acquired, cancelling (piece={}, block={})",
                    slot.index, slot.block_index
                );
                cancelled_blocks.push((slot.index, slot.begin, slot.length));
            } else {
                keep.push_back(slot);
            }
        }

        self.request_slots = keep;

        SlotCheckResult {
            timed_out,
            cancelled_blocks,
        }
    }

    // ── Upload Speed Limiting ────────────────────────────────────────────

    /// Set whether the global upload speed limit is exceeded.
    /// When true, upload messages are deferred during `drain_sendable_messages()`.
    pub fn set_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.upload_speed_exceeded = exceeded;
    }

    /// Set whether the per-group upload speed limit is exceeded.
    pub fn set_group_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.group_upload_speed_exceeded = exceeded;
    }

    /// Check if upload speed limiting is active.
    pub fn is_upload_limited(&self) -> bool {
        self.upload_speed_exceeded || self.group_upload_speed_exceeded
    }

    // ── Utility ──────────────────────────────────────────────────────────

    /// Clear all messages and request slots.
    pub fn clear(&mut self) {
        self.message_queue.clear();
        self.request_slots.clear();
    }

    /// Remove all invalidated messages from the queue.
    pub fn purge_invalidated(&mut self) {
        self.message_queue.retain(|m| !m.invalidated);
    }
}

/// Result of checking request slots for timeouts and acquired blocks.
#[derive(Debug)]
pub struct SlotCheckResult {
    /// Whether any request slot timed out (peer should be marked as snubbing).
    pub timed_out: bool,
    /// Blocks that were acquired elsewhere and need Cancel messages.
    /// Each tuple is (index, begin, length).
    pub cancelled_blocks: Vec<(u32, u32, u32)>,
}

// ======================================================================
// FloodingStat — anti-flooding detection (from C++ DefaultBtInteractive)
// ======================================================================

/// Tracks message flooding from a peer.
///
/// Mirrors C++ `FloodingStat` in `DefaultBtInteractive.h`:
/// - If >= 2 choke/unchoke transitions within 5 seconds → flooding
/// - If >= 2 keepalive messages within 5 seconds → flooding
///
/// The caller should call `check_and_reset()` periodically (every 5 seconds)
/// and disconnect the peer if `is_flooding()` returns true.
#[derive(Debug)]
pub struct FloodingStat {
    /// Number of choke/unchoke transitions in the current interval.
    choke_unchoke_count: u32,
    /// Number of keepalive messages in the current interval.
    keepalive_count: u32,
    /// Time of the last reset.
    /// `pub(crate)` for test access.
    pub(crate) last_reset: Instant,
    /// Check interval (default: 5 seconds, matching C++).
    check_interval: Duration,
    /// Threshold for choke/unchoke flooding (default: 2, matching C++).
    choke_threshold: u32,
    /// Threshold for keepalive flooding (default: 2, matching C++).
    keepalive_threshold: u32,
    /// Whether flooding was detected in the last check.
    flooding_detected: bool,
}

impl FloodingStat {
    /// Create a new flooding stat tracker with default settings.
    pub fn new() -> Self {
        Self {
            choke_unchoke_count: 0,
            keepalive_count: 0,
            last_reset: Instant::now(),
            check_interval: Duration::from_secs(5),
            choke_threshold: 2,
            keepalive_threshold: 2,
            flooding_detected: false,
        }
    }

    /// Increment choke/unchoke transition count.
    /// Call when receiving a Choke or Unchoke message that changes state.
    pub fn inc_choke_unchoke_count(&mut self) {
        self.choke_unchoke_count = self.choke_unchoke_count.saturating_add(1);
    }

    /// Get the current choke/unchoke transition count.
    /// Matches C++ `getChokeUnchokeCount()`.
    pub fn choke_unchoke_count(&self) -> u32 {
        self.choke_unchoke_count
    }

    /// Increment keepalive count.
    /// Call when receiving a KeepAlive message.
    pub fn inc_keepalive_count(&mut self) {
        self.keepalive_count = self.keepalive_count.saturating_add(1);
    }

    /// Get the current keepalive message count.
    /// Matches C++ `getKeepAliveCount()`.
    pub fn keepalive_count(&self) -> u32 {
        self.keepalive_count
    }

    /// Check if flooding is detected and reset counters if interval elapsed.
    /// Mirrors C++ `detectMessageFlooding()`.
    ///
    /// Returns true if flooding was detected.
    pub fn check_and_reset(&mut self) -> bool {
        if self.last_reset.elapsed() >= self.check_interval {
            self.flooding_detected = self.choke_unchoke_count >= self.choke_threshold
                || self.keepalive_count >= self.keepalive_threshold;

            if self.flooding_detected {
                warn!(
                    "Flooding detected: choke_unchoke={}, keepalive={}",
                    self.choke_unchoke_count, self.keepalive_count
                );
            }

            self.choke_unchoke_count = 0;
            self.keepalive_count = 0;
            self.last_reset = Instant::now();
        }
        self.flooding_detected
    }

    /// Check if flooding was detected (without resetting).
    pub fn is_flooding(&self) -> bool {
        self.flooding_detected
    }

    /// Reset all counters.
    pub fn reset(&mut self) {
        self.choke_unchoke_count = 0;
        self.keepalive_count = 0;
        self.last_reset = Instant::now();
        self.flooding_detected = false;
    }
}

impl Default for FloodingStat {
    fn default() -> Self {
        Self::new()
    }
}

// ======================================================================
// ActiveInteractionChecker — idle peer detection (from C++ DefaultBtInteractive)
// ======================================================================

/// Checks whether a peer connection should be considered inactive.
///
/// Mirrors C++ `checkActiveInteraction()` in `DefaultBtInteractive.cc`:
/// - If both sides are uninterested for 30+ seconds, disconnect gracefully.
/// - If no data exchange for 60+ seconds, disconnect.
/// - If both sides are seeders, disconnect immediately.
#[derive(Debug)]
pub struct ActiveInteractionChecker {
    /// Time when both sides became uninterested (None if either side is interested).
    pub both_uninterested_since: Option<Instant>,
    /// Time of last data exchange (request/piece message).
    pub last_data_exchange: Instant,
    /// Threshold for mutual uninterested disconnect (default: 30s, matching C++).
    pub uninterested_timeout: Duration,
    /// Threshold for total inactivity disconnect (default: 60s, matching C++).
    pub inactivity_timeout: Duration,
}

impl ActiveInteractionChecker {
    /// Create a new checker with default timeouts.
    pub fn new() -> Self {
        Self {
            both_uninterested_since: None,
            last_data_exchange: Instant::now(),
            uninterested_timeout: Duration::from_secs(30),
            inactivity_timeout: Duration::from_secs(60),
        }
    }

    /// Check if the peer connection should be considered inactive.
    ///
    /// # Arguments
    /// * `am_interested` - Whether we are interested in the peer
    /// * `peer_interested` - Whether the peer is interested in us
    /// * `we_are_seeder` - Whether we are a seeder (have all pieces)
    /// * `peer_is_seeder` - Whether the peer is a seeder (has all pieces)
    ///
    /// # Returns
    /// `InactiveReason` if the connection should be dropped, or `None` if still active.
    pub fn check(
        &mut self,
        am_interested: bool,
        peer_interested: bool,
        we_are_seeder: bool,
        peer_is_seeder: bool,
    ) -> Option<InactiveReason> {
        // Seeder-to-seeder: disconnect immediately
        if we_are_seeder && peer_is_seeder {
            return Some(InactiveReason::SeederToSeeder);
        }

        // Track mutual uninterested state
        if !am_interested && !peer_interested {
            if self.both_uninterested_since.is_none() {
                self.both_uninterested_since = Some(Instant::now());
            } else if let Some(since) = self.both_uninterested_since {
                if since.elapsed() >= self.uninterested_timeout {
                    return Some(InactiveReason::MutualUninterested);
                }
            }
        } else {
            self.both_uninterested_since = None;
        }

        // Check total inactivity
        if self.last_data_exchange.elapsed() >= self.inactivity_timeout {
            return Some(InactiveReason::NoDataExchange);
        }

        None
    }

    /// Record that a data exchange happened (request/piece message).
    pub fn record_data_exchange(&mut self) {
        self.last_data_exchange = Instant::now();
    }
}

impl Default for ActiveInteractionChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// Reason why a peer connection is considered inactive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InactiveReason {
    /// Both sides are seeders — no data exchange possible.
    SeederToSeeder,
    /// Both sides are uninterested for too long.
    MutualUninterested,
    /// No data exchange for too long.
    NoDataExchange,
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── QueuedMessage tests ────────────────────────────────────────────

    #[test]
    fn test_queued_message_control() {
        let msg = QueuedMessage::control_message(vec![1, 2, 3]);
        assert!(!msg.is_upload);
        assert!(!msg.invalidated);
        assert!(msg.piece_key.is_none());
    }

    #[test]
    fn test_queued_message_upload() {
        let msg = QueuedMessage::upload_message(
            vec![1, 2, 3],
            PieceKey {
                index: 5,
                begin: 0,
                length: 16384,
            },
        );
        assert!(msg.is_upload);
        assert!(!msg.invalidated);
        assert!(msg.piece_key.is_some());
        assert!(msg.matches_piece(5, 0, 16384));
        assert!(!msg.matches_piece(5, 0, 8192));
        assert!(msg.matches_piece_index(5));
        assert!(!msg.matches_piece_index(6));
    }

    #[test]
    fn test_queued_message_invalidation() {
        let mut msg = QueuedMessage::upload_message(
            vec![1, 2, 3],
            PieceKey {
                index: 5,
                begin: 0,
                length: 16384,
            },
        );
        assert!(!msg.invalidated);
        msg.invalidate();
        assert!(msg.invalidated);
    }

    // ── RequestSlot tests ──────────────────────────────────────────────

    #[test]
    fn test_request_slot_new() {
        let slot = RequestSlot::new(5, 0, 16384, 16384);
        assert_eq!(slot.index, 5);
        assert_eq!(slot.begin, 0);
        assert_eq!(slot.length, 16384);
        assert_eq!(slot.block_index, 0);
    }

    #[test]
    fn test_request_slot_block_index() {
        let slot = RequestSlot::new(5, 32768, 16384, 16384);
        assert_eq!(slot.block_index, 2); // 32768 / 16384 = 2
    }

    #[test]
    fn test_request_slot_matches() {
        let slot = RequestSlot::new(5, 0, 16384, 16384);
        assert!(slot.matches(5, 0, 16384));
        assert!(!slot.matches(5, 0, 8192));
        assert!(!slot.matches(6, 0, 16384));
    }

    #[test]
    fn test_request_slot_matches_block() {
        let slot = RequestSlot::new(5, 32768, 16384, 16384);
        assert!(slot.matches_block(5, 2)); // block_index = 32768 / 16384 = 2
        assert!(!slot.matches_block(5, 0));
        assert!(!slot.matches_block(6, 2));
    }

    #[test]
    fn test_request_slot_timeout() {
        let mut slot = RequestSlot::new(5, 0, 16384, 16384);
        slot.dispatched_time = Instant::now() - Duration::from_secs(120);
        assert!(slot.is_timeout(Duration::from_secs(60)));
        assert!(!slot.is_timeout(Duration::from_secs(180)));
    }

    // ── BtMessageDispatcher tests ──────────────────────────────────────

    #[test]
    fn test_dispatcher_add_and_drain_messages() {
        let mut disp = BtMessageDispatcher::new(16384);
        assert_eq!(disp.count_messages(), 0);

        disp.add_control_message(vec![0, 0, 0, 0]); // Choke
        disp.add_control_message(vec![0, 0, 0, 1]); // Unchoke
        assert_eq!(disp.count_messages(), 2);

        let sent = disp.drain_sendable_messages();
        assert_eq!(sent.len(), 2);
        assert_eq!(disp.count_messages(), 0);
    }

    #[test]
    fn test_dispatcher_upload_deferral() {
        let mut disp = BtMessageDispatcher::new(16384);

        disp.add_control_message(vec![1]);
        disp.add_upload_message(vec![2], 0, 0, 16384);
        disp.add_control_message(vec![3]);

        // Without speed limit, all messages should be sent
        let sent = disp.drain_sendable_messages();
        assert_eq!(sent.len(), 3);

        // Re-add messages with speed limit
        disp.add_control_message(vec![1]);
        disp.add_upload_message(vec![2], 0, 0, 16384);
        disp.add_control_message(vec![3]);
        disp.set_upload_speed_exceeded(true);

        // Only non-upload messages should be sent
        let sent = disp.drain_sendable_messages();
        assert_eq!(sent.len(), 2); // control messages only
        assert_eq!(disp.count_messages(), 1); // upload message still queued

        // Remove speed limit
        disp.set_upload_speed_exceeded(false);
        let sent = disp.drain_sendable_messages();
        assert_eq!(sent.len(), 1); // the deferred upload message
    }

    #[test]
    fn test_dispatcher_invalidated_messages_skipped() {
        let mut disp = BtMessageDispatcher::new(16384);

        let mut msg = QueuedMessage::control_message(vec![1, 2, 3]);
        msg.invalidate();
        disp.add_message(msg);
        disp.add_control_message(vec![4, 5, 6]);

        let sent = disp.drain_sendable_messages();
        assert_eq!(sent.len(), 1); // only the non-invalidated one
        assert_eq!(sent[0], vec![4, 5, 6]);
    }

    #[test]
    fn test_dispatcher_request_slots() {
        let mut disp = BtMessageDispatcher::new(16384);
        assert_eq!(disp.count_request_slots(), 0);

        disp.add_request_slot(5, 0, 16384);
        disp.add_request_slot(5, 16384, 16384);
        assert_eq!(disp.count_request_slots(), 2);

        assert!(disp.is_outstanding_request(5, 0));
        assert!(disp.is_outstanding_request(5, 1));
        assert!(!disp.is_outstanding_request(5, 2));
        assert!(!disp.is_outstanding_request(6, 0));

        assert!(disp.remove_request_slot(5, 0, 16384));
        assert_eq!(disp.count_request_slots(), 1);
        assert!(!disp.remove_request_slot(5, 0, 16384)); // already removed
    }

    #[test]
    fn test_dispatcher_do_choked_action() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_request_slot(5, 0, 16384);
        disp.add_request_slot(6, 0, 16384);
        disp.add_request_slot(7, 0, 16384);

        // Piece 6 is in the allowed-fast set
        let removed = disp.do_choked_action(|idx| idx == 6);
        assert_eq!(removed.len(), 2); // pieces 5 and 7 removed
        assert_eq!(disp.count_request_slots(), 1); // piece 6 kept
        assert!(disp.is_outstanding_request(6, 0));
    }

    #[test]
    fn test_dispatcher_do_choking_action() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_control_message(vec![1]); // Choke
        disp.add_upload_message(vec![2], 5, 0, 16384); // Piece
        disp.add_control_message(vec![3]); // KeepAlive
        disp.add_upload_message(vec![4], 6, 0, 16384); // Piece

        disp.do_choking_action();

        let sent = disp.drain_sendable_messages();
        // Only non-upload messages should be sent (upload messages invalidated)
        assert_eq!(sent.len(), 2); // Choke + KeepAlive
    }

    #[test]
    fn test_dispatcher_do_cancel_sending_piece_action() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_upload_message(vec![1], 5, 0, 16384);
        disp.add_upload_message(vec![2], 5, 16384, 16384);
        disp.add_upload_message(vec![3], 6, 0, 16384);

        // Cancel piece 5, offset 0, length 16384
        disp.do_cancel_sending_piece_action(5, 0, 16384);

        let sent = disp.drain_sendable_messages();
        // Only pieces 5@16384 and 6@0 should be sent
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn test_dispatcher_do_abort_outstanding_request_action() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_request_slot(5, 0, 16384);
        disp.add_request_slot(5, 16384, 16384);
        disp.add_request_slot(6, 0, 16384);

        let removed = disp.do_abort_outstanding_request_action(5);
        assert_eq!(removed.len(), 2); // both slots for piece 5
        assert_eq!(disp.count_request_slots(), 1); // piece 6 kept
    }

    #[test]
    fn test_dispatcher_check_request_slots_timeout() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_request_slot(5, 0, 16384);

        // Manually set the slot's dispatched time to the past
        if let Some(slot) = disp.request_slots.front_mut() {
            slot.dispatched_time = Instant::now() - Duration::from_secs(120);
        }

        let result = disp.check_request_slots(|_, _| false);
        assert!(result.timed_out);
        assert!(result.cancelled_blocks.is_empty());
        assert_eq!(disp.count_request_slots(), 0);
    }

    #[test]
    fn test_dispatcher_check_request_slots_acquired() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_request_slot(5, 0, 16384); // block_index = 0
        disp.add_request_slot(5, 16384, 16384); // block_index = 1

        // Block 0 of piece 5 was acquired from another peer
        let result = disp.check_request_slots(|piece, block| piece == 5 && block == 0);
        assert!(!result.timed_out);
        assert_eq!(result.cancelled_blocks.len(), 1);
        assert_eq!(result.cancelled_blocks[0], (5, 0, 16384));
        assert_eq!(disp.count_request_slots(), 1); // block 1 still there
    }

    #[test]
    fn test_dispatcher_purge_invalidated() {
        let mut disp = BtMessageDispatcher::new(16384);
        let mut msg1 = QueuedMessage::control_message(vec![1]);
        msg1.invalidate();
        disp.add_message(msg1);
        disp.add_control_message(vec![2]);

        assert_eq!(disp.count_messages(), 2);
        disp.purge_invalidated();
        assert_eq!(disp.count_messages(), 1);
    }

    #[test]
    fn test_dispatcher_clear() {
        let mut disp = BtMessageDispatcher::new(16384);
        disp.add_control_message(vec![1]);
        disp.add_request_slot(5, 0, 16384);

        disp.clear();
        assert_eq!(disp.count_messages(), 0);
        assert_eq!(disp.count_request_slots(), 0);
    }

    // ── FloodingStat tests ─────────────────────────────────────────────

    #[test]
    fn test_flooding_stat_no_flooding() {
        let mut stat = FloodingStat::new();
        stat.inc_choke_unchoke_count();
        // Only 1 transition — not flooding
        assert!(!stat.check_and_reset());
    }

    #[test]
    fn test_flooding_stat_choke_flooding() {
        let mut stat = FloodingStat::new();
        stat.inc_choke_unchoke_count();
        stat.inc_choke_unchoke_count();
        // Force the check interval to have elapsed
        stat.last_reset = Instant::now() - Duration::from_secs(6);
        assert!(stat.check_and_reset());
    }

    #[test]
    fn test_flooding_stat_keepalive_flooding() {
        let mut stat = FloodingStat::new();
        stat.inc_keepalive_count();
        stat.inc_keepalive_count();
        // Force the check interval to have elapsed
        stat.last_reset = Instant::now() - Duration::from_secs(6);
        assert!(stat.check_and_reset());
    }

    #[test]
    fn test_flooding_stat_reset_clears() {
        let mut stat = FloodingStat::new();
        stat.inc_choke_unchoke_count();
        stat.inc_choke_unchoke_count();
        stat.check_and_reset();
        // After reset, counts should be 0
        assert!(!stat.check_and_reset()); // No flooding after reset
    }

    // ── ActiveInteractionChecker tests ─────────────────────────────────

    #[test]
    fn test_active_interaction_seeder_to_seeder() {
        let mut checker = ActiveInteractionChecker::new();
        let result = checker.check(false, false, true, true);
        assert_eq!(result, Some(InactiveReason::SeederToSeeder));
    }

    #[test]
    fn test_active_interaction_normal() {
        let mut checker = ActiveInteractionChecker::new();
        let result = checker.check(true, true, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_active_interaction_mutual_uninterested() {
        let mut checker = ActiveInteractionChecker::new();
        checker.both_uninterested_since =
            Some(Instant::now() - Duration::from_secs(35));
        let result = checker.check(false, false, false, false);
        assert_eq!(result, Some(InactiveReason::MutualUninterested));
    }

    #[test]
    fn test_active_interaction_no_data_exchange() {
        let mut checker = ActiveInteractionChecker::new();
        checker.last_data_exchange = Instant::now() - Duration::from_secs(65);
        let result = checker.check(true, true, false, false);
        assert_eq!(result, Some(InactiveReason::NoDataExchange));
    }

    #[test]
    fn test_active_interaction_record_data_exchange() {
        let mut checker = ActiveInteractionChecker::new();
        checker.last_data_exchange = Instant::now() - Duration::from_secs(65);
        checker.record_data_exchange();
        let result = checker.check(true, true, false, false);
        assert!(result.is_none()); // Timer was reset
    }
}
