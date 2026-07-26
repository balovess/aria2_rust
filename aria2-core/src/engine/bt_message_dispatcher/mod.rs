//! BitTorrent Message Dispatcher — outgoing message queue and request slot tracking
//!
//! This module implements the per-peer outgoing message queue and request
//! slot management, mirroring C++ `DefaultBtMessageDispatcher`.
//!
//! # Architecture
//!
//! - [`BtMessageDispatcher`] — Per-peer dispatcher holding the outgoing message
//!   queue and outstanding request slots. Mirrors C++ `DefaultBtMessageDispatcher`.
//! - [`RequestSlot`] — Tracks a single outstanding piece-block request.
//!   Mirrors C++ `RequestSlot`.
//! - [`InactiveReason`] — Reason a peer became inactive and should be dropped.
//! - [`FloodingStat`] — Anti-flooding counters (choke/unchoke + keepalive).
//! - [`ActiveInteractionChecker`] — Timer-based inactivity detection.
//! - [`SlotCheckResult`] — Result of periodic request-slot validation.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtMessageDispatcher` | `DefaultBtMessageDispatcher` |
//! | `RequestSlot` | `RequestSlot` |
//! | `FloodingStat` | `FloodingStat` (inline in DefaultBtInteractive) |

use std::time::{Duration, Instant};

// ===========================================================================
// RequestSlot — outstanding piece-block request tracker
// ===========================================================================

/// Tracks a single outstanding Request message sent to a peer.
///
/// When we send a Request {index, begin, length} to a peer, a `RequestSlot`
/// is created to track it until the corresponding Piece message arrives
/// or the slot times out.
///
/// Mirrors C++ `RequestSlot` (defined in `RequestSlot.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSlot {
    /// Piece index
    pub index: u32,
    /// Byte offset within the piece
    pub begin: u32,
    /// Block length in bytes
    pub length: u32,
    /// Block index (= begin / block_length)
    pub block_index: u32,
    /// Command ID that created this request
    pub cuid: u64,
    /// When this request was dispatched
    pub dispatched_at: Instant,
}

impl RequestSlot {
    /// Create a new request slot.
    ///
    /// `block_size` is used to compute `block_index = begin / block_size`.
    pub fn new(index: u32, begin: u32, length: u32, block_size: u32) -> Self {
        let block_index = if block_size > 0 {
            begin / block_size
        } else {
            0
        };
        Self {
            index,
            begin,
            length,
            block_index,
            cuid: 0,
            dispatched_at: Instant::now(),
        }
    }

    /// Create a request slot with a specific command ID.
    pub fn with_cuid(index: u32, begin: u32, length: u32, block_size: u32, cuid: u64) -> Self {
        let mut slot = Self::new(index, begin, length, block_size);
        slot.cuid = cuid;
        slot
    }

    /// Check whether this slot has timed out.
    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        self.dispatched_at.elapsed() > timeout
    }

    /// Check whether this slot matches the given piece/block coordinates.
    pub fn matches(&self, index: u32, begin: u32, length: u32) -> bool {
        self.index == index && self.begin == begin && self.length == length
    }
}

// ===========================================================================
// InactiveReason — why a peer is being dropped
// ===========================================================================

/// Reason a peer became inactive and should be disconnected.
///
/// Used by `checkActiveInteraction()` in the interaction loop to decide
/// why a peer should be dropped after a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InactiveReason {
    /// Both sides uninterested for too long (30s in C++)
    MutualUninterested,
    /// No data exchanged for too long (60s total inactivity in C++)
    NoDataExchange,
    /// Both sides are seeders — no useful exchange possible
    SeederToSeeder,
}

impl std::fmt::Display for InactiveReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InactiveReason::MutualUninterested => write!(f, "mutual_uninterested"),
            InactiveReason::NoDataExchange => write!(f, "no_data_exchange"),
            InactiveReason::SeederToSeeder => write!(f, "seeder_to_seeder"),
        }
    }
}

// ===========================================================================
// FloodingStat — anti-flooding counters
// ===========================================================================

/// Anti-flooding statistics for a single peer connection.
///
/// Tracks counts of choke/unchoke transitions and keepalive messages
/// within a rolling window to detect misbehaving peers that flood
/// with control messages.
///
/// Mirrors C++ `FloodingStat` (inline struct in `DefaultBtInteractive`).
#[derive(Debug, Clone)]
pub struct FloodingStat {
    /// Count of choke/unchoke transitions in the current window
    choke_unchoke_count: u32,
    /// Count of keepalive messages in the current window
    keepalive_count: u32,
    /// Timestamp of the last flooding check/reset.
    /// Named `last_reset` in C++ aria2's FloodingStat.
    pub(crate) last_reset: Instant,
    /// Flooding check interval (default 5 seconds, matching C++)
    check_interval: Duration,
    /// Threshold for choke/unchoke flooding (2 in C++)
    choke_unchoke_threshold: u32,
    /// Threshold for keepalive flooding (2 in C++)
    keepalive_threshold: u32,
}

impl FloodingStat {
    /// Create a new flooding stat tracker with default thresholds.
    pub fn new() -> Self {
        Self {
            choke_unchoke_count: 0,
            keepalive_count: 0,
            last_reset: Instant::now(),
            check_interval: Duration::from_secs(5),
            choke_unchoke_threshold: 2,
            keepalive_threshold: 2,
        }
    }

    /// Increment the choke/unchoke counter.
    pub fn inc_choke_unchoke_count(&mut self) {
        self.choke_unchoke_count += 1;
    }

    /// Increment the keepalive counter.
    pub fn inc_keepalive_count(&mut self) {
        self.keepalive_count += 1;
    }

    /// Check for flooding and reset counters if the interval elapsed.
    ///
    /// Returns `true` if flooding was detected (peer should be disconnected).
    /// Mirrors C++ `DefaultBtInteractive::detectMessageFlooding()`.
    ///
    /// Per C++ behavior, flooding is only detected at interval boundaries.
    /// Within an interval, this always returns `false` — we must observe
    /// the full window before concluding that the peer is flooding.
    pub fn check_and_reset(&mut self) -> bool {
        if self.last_reset.elapsed() >= self.check_interval {
            let flooding = self.choke_unchoke_count >= self.choke_unchoke_threshold
                || self.keepalive_count >= self.keepalive_threshold;
            self.reset();
            flooding
        } else {
            // Within the same interval — cannot determine flooding yet.
            // C++ only checks at interval boundaries.
            false
        }
    }

    /// Reset all counters.
    pub fn reset(&mut self) {
        self.choke_unchoke_count = 0;
        self.keepalive_count = 0;
        self.last_reset = Instant::now();
    }

    /// Get current choke/unchoke count (for testing).
    pub fn choke_unchoke_count(&self) -> u32 {
        self.choke_unchoke_count
    }

    /// Get current keepalive count (for testing).
    pub fn keepalive_count(&self) -> u32 {
        self.keepalive_count
    }
}

impl Default for FloodingStat {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// ActiveInteractionChecker — inactivity detection
// ===========================================================================

/// Timer-based checker for detecting inactive peers that should be dropped.
///
/// Mirrors C++ `checkActiveInteraction()` logic in `DefaultBtInteractive`:
/// - 30s mutual-uninterested → drop
/// - 60s total inactivity → drop
/// - seeder-to-seeder → drop
#[derive(Debug, Clone)]
pub struct ActiveInteractionChecker {
    /// When the inactive timer started
    inactive_since: Instant,
    /// Mutual-uninterested timeout (default 30s)
    mutual_uninterested_timeout: Duration,
    /// Total inactivity timeout (default 60s)
    inactivity_timeout: Duration,
    /// Timestamp of the last data exchange (piece/unchoke received).
    /// Reset by `record_data_exchange()`.
    last_data_exchange: Instant,
}

impl ActiveInteractionChecker {
    /// Create a new active interaction checker with default timeouts.
    pub fn new() -> Self {
        Self {
            inactive_since: Instant::now(),
            mutual_uninterested_timeout: Duration::from_secs(30),
            inactivity_timeout: Duration::from_secs(60),
            last_data_exchange: Instant::now(),
        }
    }

    /// Check whether the peer should be dropped due to inactivity.
    ///
    /// Returns `Some(InactiveReason)` if the peer should be disconnected,
    /// or `None` if the peer is still considered active.
    pub fn check(
        &self,
        am_interested: bool,
        peer_interested: bool,
        is_seeder: bool,
        peer_is_seeder: bool,
    ) -> Option<InactiveReason> {
        let elapsed = self.inactive_since.elapsed();

        // Seeder-to-seeder: no possible data exchange
        if is_seeder && peer_is_seeder {
            return Some(InactiveReason::SeederToSeeder);
        }

        // Mutual uninterested timeout
        if !am_interested && !peer_interested && elapsed >= self.mutual_uninterested_timeout {
            return Some(InactiveReason::MutualUninterested);
        }

        // Total inactivity timeout
        if elapsed >= self.inactivity_timeout {
            return Some(InactiveReason::NoDataExchange);
        }

        None
    }

    /// Reset the inactive timer (called when data is exchanged).
    pub fn reset_timer(&mut self) {
        self.inactive_since = Instant::now();
    }

    /// Record that a data exchange occurred (piece/unchoke received).
    /// Resets the inactivity timer.
    pub fn record_data_exchange(&mut self) {
        self.last_data_exchange = Instant::now();
    }
}

impl Default for ActiveInteractionChecker {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// SlotCheckResult — result of periodic request-slot validation
// ===========================================================================

/// Result of checking outstanding request slots for timeouts and
/// already-acquired blocks.
///
/// Returned by `BtMessageDispatcher::check_request_slots()`.
#[derive(Debug, Clone)]
pub struct SlotCheckResult {
    /// Whether any request slot timed out (peer may be snubbing)
    pub timed_out: bool,
    /// Blocks that have been acquired from another peer and need Cancel messages.
    /// Each tuple is (piece_index, begin, length).
    pub cancelled_blocks: Vec<(u32, u32, u32)>,
}

impl Default for SlotCheckResult {
    fn default() -> Self {
        Self {
            timed_out: false,
            cancelled_blocks: Vec::new(),
        }
    }
}

// ===========================================================================
// PendingMessage — tagged outgoing message
// ===========================================================================

/// A tagged outgoing message in the per-peer send queue.
///
/// The tag distinguishes upload (Piece) messages from control messages
/// (Choke, Unchoke, Request, Cancel, etc.) so that:
///
/// - `do_choking_action()` can remove all upload messages when we choke the peer.
/// - `do_cancel_sending_piece_action()` can remove a specific upload message.
/// - `drain_sendable_messages()` can defer upload messages when upload speed
///   is limited (mirroring C++ `sendMessagesInternal()` which checks
///   `isUploading()` per message).
///
/// Mirrors C++ `BtMessage::isUploading()` which returns true for Piece messages.
#[derive(Debug)]
enum PendingMessage {
    /// A non-upload control message (Choke, Unchoke, Request, Cancel, etc.)
    Control(Vec<u8>),
    /// A Piece upload message carrying data for the specified block.
    Upload {
        data: Vec<u8>,
        index: u32,
        begin: u32,
        length: u32,
    },
}

// ===========================================================================
// BtMessageDispatcher — per-peer outgoing message queue + request slots
// ===========================================================================

/// Per-peer message dispatcher holding the outgoing message queue and
/// outstanding request slots.
///
/// Mirrors C++ `DefaultBtMessageDispatcher`. Each peer connection has
/// one dispatcher instance that:
/// - Queues outgoing messages (Request, Piece, Choke, Unchoke, etc.)
/// - Tracks outstanding Request slots until Piece responses arrive
/// - Handles choke/unchoke side effects on the request queue
/// - Validates slots for timeouts and already-acquired blocks
#[derive(Debug)]
pub struct BtMessageDispatcher {
    /// Block size for computing block indices
    block_size: u32,
    /// Outstanding request slots (pub(crate) for test access)
    pub(crate) request_slots: Vec<RequestSlot>,
    /// Outgoing message queue with upload/control tagging.
    /// Mirrors C++ `messageQueue_` which holds `unique_ptr<BtMessage>`.
    message_queue: Vec<PendingMessage>,
    /// Whether the global upload speed limit is exceeded
    upload_speed_exceeded: bool,
    /// Whether the per-group upload speed limit is exceeded
    group_upload_speed_exceeded: bool,
    /// Request timeout (default 60s, matching C++ `requestTimeout_`)
    request_timeout: Duration,
}

impl BtMessageDispatcher {
    /// Create a new dispatcher with the given block size.
    pub fn new(block_size: u32) -> Self {
        Self {
            block_size,
            request_slots: Vec::new(),
            message_queue: Vec::new(),
            upload_speed_exceeded: false,
            group_upload_speed_exceeded: false,
            request_timeout: Duration::from_secs(60),
        }
    }

    /// Add a request slot to track an outstanding Request message.
    pub fn add_request_slot(&mut self, index: u32, begin: u32, length: u32) {
        let slot = RequestSlot::new(index, begin, length, self.block_size);
        self.request_slots.push(slot);
    }

    /// Remove a request slot matching the given piece/block coordinates.
    ///
    /// Returns `true` if a matching slot was found and removed.
    pub fn remove_request_slot(&mut self, index: u32, begin: u32, length: u32) -> bool {
        if let Some(pos) = self
            .request_slots
            .iter()
            .position(|s| s.matches(index, begin, length))
        {
            self.request_slots.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Add a serialized Request message to the outgoing queue.
    pub fn add_request_message(&mut self, data: Vec<u8>, _index: u32, _begin: u32, _length: u32) {
        self.message_queue.push(PendingMessage::Control(data));
    }

    /// Add a control message (non-upload) to the outgoing queue.
    pub fn add_control_message(&mut self, data: Vec<u8>) {
        self.message_queue.push(PendingMessage::Control(data));
    }

    /// Add a Piece upload message to the outgoing queue.
    ///
    /// Unlike the previous stub that silently dropped messages when upload
    /// was limited, this always queues the message. Upload speed filtering
    /// is applied at drain time, mirroring C++ `sendMessagesInternal()`
    /// which defers upload messages when speed is exceeded.
    pub fn add_upload_message(&mut self, data: Vec<u8>, index: u32, begin: u32, length: u32) {
        self.message_queue.push(PendingMessage::Upload {
            data,
            index,
            begin,
            length,
        });
    }

    /// Handle a Choke message received from the peer.
    ///
    /// Removes outstanding request slots for pieces NOT in the allowed-fast set.
    /// Returns removed slots so the caller can send Cancel messages.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokedAction()`.
    pub fn do_choked_action<F>(&mut self, is_in_allowed_fast: F) -> Vec<RequestSlot>
    where
        F: Fn(u32) -> bool,
    {
        let mut removed = Vec::new();
        self.request_slots.retain(|slot| {
            if is_in_allowed_fast(slot.index) {
                true // Keep: piece is in allowed-fast set
            } else {
                removed.push(slot.clone());
                false
            }
        });
        removed
    }

    /// Handle sending a Choke message to the peer.
    ///
    /// Removes all queued Piece upload messages since we are choking
    /// the peer and should not send them data.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokingAction()` which
    /// calls `onChokingEvent()` on each queued message. For Piece messages,
    /// `onChokingEvent()` invalidates them (marks as not-to-send).
    pub fn do_choking_action(&mut self) {
        self.message_queue
            .retain(|msg| !matches!(msg, PendingMessage::Upload { .. }));
    }

    /// Handle receiving a Cancel message from the peer.
    ///
    /// Removes any queued Piece upload message that matches the specified block.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doCancelSendingPieceAction()`
    /// which calls `onCancelSendingPieceEvent()` on each queued message.
    /// For a matching Piece message, this invalidates it.
    pub fn do_cancel_sending_piece_action(&mut self, index: u32, begin: u32, length: u32) {
        self.message_queue.retain(|msg| {
            !matches!(msg, PendingMessage::Upload {
                index: idx,
                begin: b,
                length: l,
                ..
            } if *idx == index && *b == begin && *l == length)
        });
    }

    /// Abort all outstanding requests for the given piece index.
    ///
    /// Called when a piece is reassigned to another peer. Returns
    /// removed request slots.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doAbortOutstandingRequestAction()`.
    pub fn do_abort_outstanding_request_action(&mut self, piece_index: u32) -> Vec<RequestSlot> {
        let mut removed = Vec::new();
        self.request_slots.retain(|slot| {
            if slot.index == piece_index {
                removed.push(slot.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Check request slots for timeouts and already-acquired blocks.
    ///
    /// Should be called approximately once per second. Returns a
    /// [`SlotCheckResult`] indicating timed-out slots and blocks
    /// that need Cancel messages.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::checkRequestSlotAndDoNecessaryThing()`.
    pub fn check_request_slots<F>(&mut self, is_block_acquired: F) -> SlotCheckResult
    where
        F: Fn(u32, u32) -> bool,
    {
        let mut result = SlotCheckResult::default();
        let timeout = self.request_timeout;

        self.request_slots.retain(|slot| {
            // Check for timeout
            if slot.is_timed_out(timeout) {
                result.timed_out = true;
                return false;
            }
            // Check if block was acquired from another peer
            if is_block_acquired(slot.index, slot.block_index) {
                result
                    .cancelled_blocks
                    .push((slot.index, slot.begin, slot.length));
                return false;
            }
            true
        });

        result
    }

    /// Count outstanding request slots.
    pub fn count_request_slots(&self) -> usize {
        self.request_slots.len()
    }

    /// Check if there are any outstanding request slots.
    pub fn has_outstanding_requests(&self) -> bool {
        !self.request_slots.is_empty()
    }

    /// Check if there is an outstanding request for the given piece+block.
    pub fn is_outstanding_request(&self, index: u32, block_index: u32) -> bool {
        self.request_slots
            .iter()
            .any(|s| s.index == index && s.block_index == block_index)
    }

    /// Drain messages that are ready to be sent.
    ///
    /// Upload (Piece) messages are deferred when upload speed limiting is
    /// active; they remain in the queue for a later drain attempt.
    /// Control messages are always drained.
    ///
    /// Mirrors C++ `sendMessagesInternal()` which checks `isUploading()`
    /// per message and moves upload messages to `tempQueue` when speed
    /// limits are exceeded, then re-inserts them at the front of the queue.
    pub fn drain_sendable_messages(&mut self) -> Vec<Vec<u8>> {
        let mut sendable = Vec::new();
        let mut deferred = Vec::new();

        // Cache the upload-limited flag before draining to avoid borrowing `self`
        // while iterating over `self.message_queue`.
        let upload_limited = self.is_upload_limited();

        for msg in self.message_queue.drain(..) {
            match msg {
                PendingMessage::Control(data) => {
                    sendable.push(data);
                }
                PendingMessage::Upload {
                    data,
                    index,
                    begin,
                    length,
                } => {
                    if upload_limited {
                        // Defer: re-queue the upload message
                        deferred.push(PendingMessage::Upload {
                            data,
                            index,
                            begin,
                            length,
                        });
                    } else {
                        sendable.push(data);
                    }
                }
            }
        }

        // Re-insert deferred upload messages (preserving order, matching C++
        // which inserts tempQueue at the front of messageQueue_).
        if !deferred.is_empty() {
            deferred.extend(self.message_queue.drain(..));
            self.message_queue = deferred;
        }

        sendable
    }

    /// Check if there are pending messages in the queue.
    pub fn has_pending_messages(&self) -> bool {
        !self.message_queue.is_empty()
    }

    /// Count messages in the outgoing queue.
    pub fn count_messages(&self) -> usize {
        self.message_queue.len()
    }

    /// Set whether the global upload speed limit is exceeded.
    pub fn set_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.upload_speed_exceeded = exceeded;
    }

    /// Set whether the per-group upload speed limit is exceeded.
    pub fn set_group_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.group_upload_speed_exceeded = exceeded;
    }

    /// Check if upload speed limiting is active (either global or per-group).
    pub fn is_upload_limited(&self) -> bool {
        self.upload_speed_exceeded || self.group_upload_speed_exceeded
    }

    /// Clear all messages and request slots.
    pub fn clear(&mut self) {
        self.request_slots.clear();
        self.message_queue.clear();
        self.upload_speed_exceeded = false;
        self.group_upload_speed_exceeded = false;
    }
}
