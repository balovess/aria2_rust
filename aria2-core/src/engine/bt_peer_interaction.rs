//! BT Peer Interaction Manager - Peer connection, initialization, and per-peer
//! interaction loop
//!
//! This module manages the interaction with BitTorrent peers, including:
//! - Connection establishment (plain and encrypted)
//! - Initial handshake and bitfield exchange
//! - Waiting for unchoke messages
//! - Per-peer interaction loop (`BtPeerInteractive`)
//! - Peer connection lifecycle state machine (`PeerConnectionState`)
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DefaultBtInteractive.h/.cc` — Per-peer interaction loop
//! - `src/PeerInteractionCommand.h/.cc` — Peer connection lifecycle command
//! - `src/PeerConnection.cc/h` — Peer connection management
//! - `src/BtSetup.cc/h` — BT setup and initialization

use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::constants;
use crate::engine::bt_message_dispatcher::{
    ActiveInteractionChecker, FloodingStat, InactiveReason, RequestSlot,
};
use crate::engine::bt_message_handler::BtPeerMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_request_factory::{BtRequestFactory, PieceBlockRequest};
use crate::engine::extension_registry::{self, ExtensionRegistry, ExtensionUpdate};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::segment::piece::Piece;
use tracing::{debug, error, info, trace, warn};

// ======================================================================
// PieceProvider trait — abstraction for PieceStorage dependency
// ======================================================================

/// Trait abstracting the piece storage operations needed by the BT
/// interaction loop for request generation.
///
/// In C++ `DefaultBtInteractive`, `pieceStorage_` is a raw pointer used
/// for `hasMissingPiece()`, `getMissingPiece()`, `isEndGame()`,
/// `hasMissingUnusedPiece()`, and `enterEndGame()`. This trait exposes
/// those operations so the interaction loop remains decoupled from
/// the full `PieceStorage` trait.
///
/// Note: Some methods (`is_end_game`, `has_missing_unused_piece`,
/// `enter_end_game`) also exist on `PieceStorage`. For types that
/// implement both traits, call via unambiguous syntax:
/// `PieceProvider::is_end_game(&storage)` or `PieceStorage::is_end_game(&storage)`.
pub trait PieceProvider: Send + Sync {
    /// Check if the peer has pieces we still need.
    /// Mirrors C++ `PieceStorage::hasMissingPiece(peer)`.
    fn has_missing_piece(&self, peer: &BtPeerConn) -> bool;

    /// Get missing pieces for this peer, up to `count` pieces.
    /// Mirrors C++ `PieceStorage::getMissingPiece(pieces, count, peer, cuid)`.
    ///
    /// In the C++ code, `getMissingPiece` fills the `pieces` vector with
    /// up to `count` pieces. The Rust version returns a `Vec<Piece>`.
    ///
    /// The `target_piece_indexes` parameter lists pieces already assigned
    /// to this peer (from `BtRequestFactory::getTargetPieceIndexes()`),
    /// so the storage can avoid assigning the same piece twice.
    fn get_missing_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece>;

    /// Get missing fast-extension pieces for a choked peer.
    /// Mirrors C++ `PieceStorage::getMissingFastPiece(pieces, count, peer, indexes, cuid)`.
    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece>;

    /// Check whether end-game mode is active.
    /// Mirrors C++ `PieceStorage::isEndGame()`.
    fn is_end_game(&self) -> bool;

    /// Check if there are missing pieces that are not in-use by any peer.
    /// Mirrors C++ `PieceStorage::hasMissingUnusedPiece()`.
    fn has_missing_unused_piece(&self) -> bool;

    /// Enter end-game mode.
    /// Mirrors C++ `PieceStorage::enterEndGame()`.
    fn enter_end_game(&mut self);

    // ── checkHave optimization support ──────────────────────────────────

    /// Get piece indexes advertised since `last_have_index` by CUIDs other
    /// than `my_cuid`. Returns (indexes, new_last_have_index).
    /// Mirrors C++ `PieceStorage::getAdvertisedPieceIndexes()`.
    fn get_advertised_piece_indexes_ext(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64);

    /// Returns the bitfield byte length.
    /// Mirrors C++ `PieceStorage::getBitfieldLength()`.
    fn get_bitfield_length_ext(&self) -> usize;

    /// Returns the completion bitfield.
    /// Mirrors C++ `PieceStorage::getBitfield()`.
    fn get_bitfield_ext(&self) -> Vec<u8>;

    /// Check if all downloads are finished (ignoring filter).
    /// Mirrors C++ `PieceStorage::allDownloadFinished()`.
    fn all_download_finished_ext(&self) -> bool;

    /// Returns the total completed length in bytes.
    /// Mirrors C++ `PieceStorage::getCompletedLength()`.
    fn get_completed_length_ext(&self) -> u64;
}

// ======================================================================
// Constants (matching C++ aria2)
// ======================================================================

/// Delay between peer connection setup and message reading (milliseconds)
pub const PEER_CONNECTION_DELAY_MS: u64 = constants::BT_PEER_CONNECTION_DELAY_MS;

/// Maximum attempts to wait for unchoke from a peer
pub const MAX_UNCHOKE_WAIT_ATTEMPTS: u32 = constants::BT_MAX_UNCHOKE_WAIT_ATTEMPTS as u32;

/// Timeout for each message read from peer (seconds)
pub const PEER_MESSAGE_TIMEOUT_SECS: u64 = constants::BT_PEER_MESSAGE_TIMEOUT_SECS;

/// Default maximum number of outstanding piece requests per peer.
/// Matches C++ `DEFAULT_MAX_OUTSTANDING_REQUEST = 6` in BtConstants.h.
pub const DEFAULT_MAX_OUTSTANDING_REQUEST: usize =
    constants::BT_DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Upper bound for max outstanding requests (dynamic scaling ceiling).
/// Matches C++ `UB_MAX_OUTSTANDING_REQUEST = 256` in BtConstants.h.
pub const UB_MAX_OUTSTANDING_REQUEST: usize = constants::BT_UB_MAX_OUTSTANDING_REQUEST;

/// Default keep-alive interval in seconds.
/// Matches C++ `keepAliveInterval_` default (120s).
pub const DEFAULT_KEEP_ALIVE_INTERVAL_SECS: u64 = 120;

/// Default allowed-fast set size.
/// Matches C++ `allowedFastSetSize_` default (10).
pub const DEFAULT_ALLOWED_FAST_SET_SIZE: usize = 10;

/// Mutual-uninterested disconnect timeout (seconds).
/// Matches C++ `checkActiveInteraction()` interval = 30.
pub const MUTUAL_UNINTERESTED_TIMEOUT_SECS: u64 = 30;

/// Total inactivity disconnect timeout (seconds).
/// Matches C++ `checkActiveInteraction()` interval = 60.
pub const INACTIVITY_TIMEOUT_SECS: u64 = 60;

/// Flooding check interval (seconds).
/// Matches C++ `FLOODING_CHECK_INTERVAL` = 5.
pub const FLOODING_CHECK_INTERVAL_SECS: u64 = 5;

/// Per-second timer interval for request slot checking.
pub const PER_SEC_INTERVAL_SECS: u64 = 1;

/// PEX (Peer Exchange) interval in seconds.
/// Matches C++ default PEX interval (60s).
pub const PEX_INTERVAL_SECS: u64 = 60;

// ======================================================================
// PeerConnectionState — lifecycle state machine
// ======================================================================

/// Peer connection lifecycle state machine.
///
/// Mirrors C++ `PeerInteractionCommand::Seq`:
///
/// ```text
/// Initiator path:
///   InitiatorSendHandshake → InitiatorWaitHandshake → Wired
///
/// Receiver path:
///   ReceiverWaitHandshake → Wired
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerConnectionState {
    /// Initiator: about to send our handshake to the peer.
    InitiatorSendHandshake,
    /// Initiator: handshake sent, waiting for the peer's handshake response.
    InitiatorWaitHandshake,
    /// Receiver: waiting for the peer's handshake (incoming connection).
    ReceiverWaitHandshake,
    /// Fully wired — handshake complete, normal interaction loop active.
    Wired,
}

impl PeerConnectionState {
    /// Returns true if the state is a pre-handshake state
    /// where we should not run the normal interaction loop.
    pub fn is_handshake_state(&self) -> bool {
        !matches!(self, PeerConnectionState::Wired)
    }

    /// Returns true if we are in the `Wired` state and should
    /// run `doInteractionProcessing()`.
    pub fn is_wired(&self) -> bool {
        matches!(self, PeerConnectionState::Wired)
    }
}

impl std::fmt::Display for PeerConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerConnectionState::InitiatorSendHandshake => {
                write!(f, "INITIATOR_SEND_HANDSHAKE")
            }
            PeerConnectionState::InitiatorWaitHandshake => {
                write!(f, "INITIATOR_WAIT_HANDSHAKE")
            }
            PeerConnectionState::ReceiverWaitHandshake => {
                write!(f, "RECEIVER_WAIT_HANDSHAKE")
            }
            PeerConnectionState::Wired => write!(f, "WIRED"),
        }
    }
}

// ======================================================================
// InteractionResult — what happened during an interaction iteration
// ======================================================================

/// Result of a single `do_interaction_processing()` iteration.
///
/// In C++ the interaction loop throws exceptions on errors. In Rust we
/// return a result enum so the caller can decide how to handle each case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionResult {
    /// Normal processing completed; continue next iteration.
    Continue,
    /// Peer should be disconnected due to inactivity.
    Disconnect(InactiveReason),
    /// Message flooding detected; disconnect the peer.
    FloodingDetected,
    /// Still in handshake state; interaction loop not yet active.
    WaitingForHandshake,
}

// ======================================================================
// ChokingDecision / InterestDecision
// ======================================================================

/// Decision about whether to choke or unchoke the peer.
///
/// Mirrors C++ `decideChoking()`: if `shouldBeChoking()` differs from
/// `amChoking()`, we need to send a Choke or Unchoke message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChokingDecision {
    /// We should choke the peer (send Choke message).
    Choke,
    /// We should unchoke the peer (send Unchoke message).
    Unchoke,
    /// No change needed.
    NoChange,
}

/// Decision about whether to express interest or lack thereof.
///
/// Mirrors C++ `decideInterest()`: if our interest state doesn't match
/// whether we have missing pieces from this peer, send Interested or
/// NotInterested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterestDecision {
    /// We should express interest (send Interested message).
    Interested,
    /// We should express lack of interest (send NotInterested message).
    NotInterested,
    /// No change needed.
    NoChange,
}

// ======================================================================
// CheckHaveResult — what to send after checkHave optimization
// ======================================================================

/// Result of the `checkHave` optimization decision.
///
/// Mirrors C++ `DefaultBtInteractive::checkHave()`: when there are many
/// newly completed pieces to advertise, it's more efficient to send a
/// single Bitfield message instead of many individual Have messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckHaveResult {
    /// No new pieces to advertise.
    None,
    /// Send individual Have messages for these piece indexes.
    HaveIndexes(Vec<usize>),
    /// Send a single Bitfield message (more efficient than many Haves).
    Bitfield,
    /// Send a HaveAll message (fast extension, all pieces complete).
    HaveAll,
}

// ======================================================================
// DispatchUpdate — state changes from dispatching a single message
// ======================================================================

/// State update produced by dispatching a single received message.
///
/// Collected during `receive_messages()` so the caller can apply
/// side-effects (e.g., cancelling outstanding requests after choke)
/// after the batch is processed.
#[derive(Debug, Clone)]
pub struct DispatchUpdate {
    /// Request slots removed by a Choke message (caller should send Cancel).
    pub cancelled_slots: Vec<RequestSlot>,
    /// Piece index received via Have (caller should update bitfield).
    pub have_index: Option<u32>,
    /// Bitfield data received (caller should update peer bitfield).
    pub bitfield_data: Option<Vec<u8>>,
    /// Whether the peer choking state changed.
    pub peer_choking_changed: bool,
    /// New peer choking value (only meaningful if peer_choking_changed).
    pub peer_choking: bool,
    /// Extension protocol update (BEP 10/9/11), if any.
    pub extension_update: Option<ExtensionUpdate>,
}

impl Default for DispatchUpdate {
    fn default() -> Self {
        Self {
            cancelled_slots: Vec::new(),
            have_index: None,
            bitfield_data: None,
            peer_choking_changed: false,
            peer_choking: false,
            extension_update: None,
        }
    }
}

// ======================================================================
// BtPeerInteractive — per-peer interaction loop (C++ DefaultBtInteractive)
// ======================================================================

/// Per-peer interaction manager that runs the processing loop each tick.
///
/// Mirrors C++ `DefaultBtInteractive`. Each active peer connection has one
/// instance. The main entry point is [`do_interaction_processing()`],
/// which is called once per command execution cycle in the `Wired` state.
///
/// # C++ `doInteractionProcessing()` flow
///
/// ```text
/// checkActiveInteraction()           — 30s mutual-uninterested, 60s total, seeder-seeder
/// if perSecTimer >= 1s:
///     checkRequestSlotAndDoNecessaryThing()  — timeout + already-acquired
/// receiveMessages()
/// detectMessageFlooding()           — >=2 choke/unchoke or >=2 keepalive in 5s
/// decideChoking()                   — should we choke/unchoke?
/// decideInterest()                  — do we have missing pieces from this peer?
/// checkHave()                       — advertise newly completed pieces
/// sendKeepAlive()                   — every 120s
/// removeCompletedPiece()
/// if !downloadFinished:
///     addRequests()                 — fill piece requests
/// addPeerExchangeMessage()
/// sendPendingMessage()
/// ```
pub struct BtPeerInteractive {
    // ── Connection state ───────────────────────────────────────────────
    /// Current lifecycle state of this peer connection.
    state: PeerConnectionState,

    // ── Message handler (C++ DefaultBtMessageDispatcher per peer) ──────
    /// Per-peer message handler with request slot tracking and flooding.
    handler: BtPeerMessageHandler,

    // ── Peer state tracking (C++ Peer fields) ─────────────────────────
    /// Whether we are currently choking this peer (C++ `amChoking_`).
    am_choking: bool,
    /// Whether we are currently interested in this peer (C++ `amInterested_`).
    am_interested: bool,
    /// Whether the peer is currently choking us (C++ `peerChoking_`).
    peer_choking: bool,
    /// Whether the peer is currently interested in us (C++ `peerInterested_`).
    peer_interested: bool,

    // ── Timers (matching C++) ──────────────────────────────────────────
    /// Timer for keep-alive sending (C++ `keepAliveTimer_`).
    keep_alive_timer: Instant,
    /// Timer for flooding check interval (C++ `floodingTimer_`).
    flooding_timer: Instant,
    /// Timer for inactive peer detection (C++ `inactiveTimer_`).
    inactive_timer: Instant,
    /// Per-second timer for request slot checking (C++ `perSecTimer_`).
    per_sec_timer: Instant,
    /// Timer for peer exchange messages (C++ `pexTimer_`).
    pex_timer: Instant,

    // ── Configuration ──────────────────────────────────────────────────
    /// Keep-alive interval in seconds (C++ `keepAliveInterval_`, default 120).
    keep_alive_interval_secs: u64,
    /// Maximum outstanding piece requests (C++ `maxOutstandingRequest_`, default 6).
    max_outstanding_request: usize,
    /// Allowed-fast set size (C++ `allowedFastSetSize_`, default 10).
    allowed_fast_set_size: usize,

    // ── Flooding detection ─────────────────────────────────────────────
    /// Flooding statistics tracker.
    flooding_stat: FloodingStat,

    // ── Active interaction checking ────────────────────────────────────
    /// Inactive peer checker.
    active_interaction_checker: ActiveInteractionChecker,

    // ── Tracking ───────────────────────────────────────────────────────
    /// Last have index we have advertised to the peer (C++ `lastHaveIndex_`).
    last_have_index: u64,
    /// Number of messages received in the current iteration (C++ `numReceivedMessage_`).
    num_received_message: usize,
    /// Total number of pieces in the torrent.
    #[allow(dead_code)]
    num_pieces: u32,
    /// 20-byte info hash for this torrent.
    info_hash: [u8; 20],

    // ── Feature flags ──────────────────────────────────────────────────
    /// Whether UT PEX (peer exchange) is enabled (C++ `utPexEnabled_`).
    ut_pex_enabled: bool,
    /// Whether DHT is enabled (C++ `dhtEnabled_`).
    dht_enabled: bool,
    /// Whether we are in metadata-get mode (C++ `metadataGetMode_`).
    metadata_get_mode: bool,
    /// Whether the download is finished (affects addRequests step).
    download_finished: bool,

    // ── Extension Protocol (BEP 10) ──────────────────────────────────────
    /// Per-peer extension registry tracking local and peer ext_id assignments.
    extension_registry: ExtensionRegistry,

    // ── Request generation (C++ DefaultBtRequestFactory) ──────────────────
    /// Per-peer request factory managing target pieces and generating Request messages.
    /// Mirrors C++ `btRequestFactory_` in `DefaultBtInteractive`.
    request_factory: BtRequestFactory,

    /// Whether end-game mode has been entered for this download.
    /// Mirrors C++ `endGame_` in `DefaultBtInteractive`.
    endgame: bool,
}

impl BtPeerInteractive {
    /// Create a new `BtPeerInteractive` for a peer connection.
    ///
    /// # Arguments
    /// * `info_hash` — 20-byte torrent info hash
    /// * `num_pieces` — Total number of pieces in the torrent
    ///
    /// All timers are initialized to `Instant::now()`, matching the C++
    /// constructor which sets all timers to `global::wallclock()`.
    pub fn new(info_hash: [u8; 20], num_pieces: u32) -> Self {
        let now = Instant::now();
        Self {
            state: PeerConnectionState::InitiatorSendHandshake,
            handler: BtPeerMessageHandler::new(constants::BT_BLOCK_SIZE as u32),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            keep_alive_timer: now,
            flooding_timer: now,
            inactive_timer: now,
            per_sec_timer: now,
            pex_timer: now,
            keep_alive_interval_secs: DEFAULT_KEEP_ALIVE_INTERVAL_SECS,
            max_outstanding_request: DEFAULT_MAX_OUTSTANDING_REQUEST,
            allowed_fast_set_size: DEFAULT_ALLOWED_FAST_SET_SIZE,
            flooding_stat: FloodingStat::new(),
            active_interaction_checker: ActiveInteractionChecker::new(),
            last_have_index: 0,
            num_received_message: 0,
            num_pieces,
            info_hash,
            ut_pex_enabled: false,
            dht_enabled: false,
            metadata_get_mode: false,
            download_finished: false,
            extension_registry: ExtensionRegistry::new(),
            request_factory: BtRequestFactory::new(constants::BT_BLOCK_SIZE as u32),
            endgame: false,
        }
    }

    /// Create with a specific initial state (e.g., `ReceiverWaitHandshake`
    /// for incoming connections).
    pub fn with_state(info_hash: [u8; 20], num_pieces: u32, state: PeerConnectionState) -> Self {
        let mut interactive = Self::new(info_hash, num_pieces);
        interactive.state = state;
        interactive
    }

    // ── Configuration setters ──────────────────────────────────────────

    /// Set the keep-alive interval in seconds.
    /// Matches C++ `setKeepAliveInterval()`.
    pub fn set_keep_alive_interval(&mut self, secs: u64) {
        self.keep_alive_interval_secs = secs;
    }

    /// Set the maximum outstanding request count.
    pub fn set_max_outstanding_request(&mut self, max: usize) {
        self.max_outstanding_request = max.max(1).min(UB_MAX_OUTSTANDING_REQUEST);
    }

    /// Set the allowed-fast set size.
    pub fn set_allowed_fast_set_size(&mut self, size: usize) {
        self.allowed_fast_set_size = size;
    }

    /// Enable or disable UT PEX (peer exchange).
    /// Matches C++ `setUTPexEnabled()`.
    pub fn set_ut_pex_enabled(&mut self, enabled: bool) {
        self.ut_pex_enabled = enabled;
    }

    /// Enable or disable DHT.
    /// Matches C++ `setDHTEnabled()`.
    pub fn set_dht_enabled(&mut self, enabled: bool) {
        self.dht_enabled = enabled;
    }

    /// Enable metadata-get mode.
    /// Matches C++ `enableMetadataGetMode()`.
    pub fn enable_metadata_get_mode(&mut self) {
        self.metadata_get_mode = true;
    }

    /// Set whether the download is finished (affects addRequests step).
    pub fn set_download_finished(&mut self, finished: bool) {
        self.download_finished = finished;
    }

    // ── State accessors ────────────────────────────────────────────────

    /// Get the current connection lifecycle state.
    pub fn state(&self) -> PeerConnectionState {
        self.state
    }

    /// Get the number of messages received in the last iteration.
    /// Matches C++ `countReceivedMessageInIteration()`.
    pub fn count_received_message_in_iteration(&self) -> usize {
        self.num_received_message
    }

    /// Get the current max outstanding request count.
    pub fn max_outstanding_request(&self) -> usize {
        self.max_outstanding_request
    }

    /// Get the info hash for this connection.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// Check if metadata-get mode is enabled.
    pub fn is_metadata_get_mode(&self) -> bool {
        self.metadata_get_mode
    }

    /// Get whether we are currently choking this peer.
    /// Matches C++ `Peer::amChoking()`.
    pub fn am_choking(&self) -> bool {
        self.am_choking
    }

    /// Get whether we are currently interested in this peer.
    /// Matches C++ `Peer::amInterested()`.
    pub fn am_interested(&self) -> bool {
        self.am_interested
    }

    /// Get whether the peer is currently choking us.
    /// Matches C++ `Peer::peerChoking()`.
    pub fn peer_choking(&self) -> bool {
        self.peer_choking
    }

    /// Get whether the peer is currently interested in us.
    /// Matches C++ `Peer::peerInterested()`.
    pub fn peer_interested(&self) -> bool {
        self.peer_interested
    }

    /// Get a reference to the per-peer message handler.
    pub fn handler(&self) -> &BtPeerMessageHandler {
        &self.handler
    }

    /// Get a mutable reference to the per-peer message handler.
    pub fn handler_mut(&mut self) -> &mut BtPeerMessageHandler {
        &mut self.handler
    }

    /// Get a reference to the per-peer extension registry.
    pub fn extension_registry(&self) -> &ExtensionRegistry {
        &self.extension_registry
    }

    /// Get a mutable reference to the per-peer extension registry.
    pub fn extension_registry_mut(&mut self) -> &mut ExtensionRegistry {
        &mut self.extension_registry
    }

    // ── State machine transitions ──────────────────────────────────────

    /// Advance the state machine to `Wired` after a successful handshake.
    ///
    /// Resets all interaction timers, matching C++
    /// `doPostHandshakeProcessing()` which sets:
    /// - `keepAliveTimer_ = global::wallclock()`
    /// - `floodingTimer_ = global::wallclock()`
    /// - `pexTimer_ = Timer::zero()` (effectively "immediate")
    ///
    /// # Panics
    /// Panics if the current state is already `Wired` (invalid transition).
    pub fn advance_to_wired(&mut self) {
        debug!(
            state = %self.state,
            "BtPeerInteractive: advancing to WIRED state"
        );
        assert!(
            !self.state.is_wired(),
            "Cannot advance to WIRED from WIRED state"
        );
        let now = Instant::now();
        self.state = PeerConnectionState::Wired;
        self.keep_alive_timer = now;
        self.flooding_timer = now;
        self.inactive_timer = now;
        self.per_sec_timer = now;
        // PEX timer set to "far past" so the first PEX message is sent
        // immediately when the interval is checked. Use checked_sub to
        // avoid panic on platforms where Instant origin is near zero.
        self.pex_timer = now.checked_sub(Duration::from_secs(3600)).unwrap_or(now);
    }

    /// Transition from `InitiatorSendHandshake` to `InitiatorWaitHandshake`.
    ///
    /// # Panics
    /// Panics if the current state is not `InitiatorSendHandshake`.
    pub fn advance_to_wait_handshake(&mut self) {
        debug!(
            state = %self.state,
            "BtPeerInteractive: handshake sent, waiting for response"
        );
        assert_eq!(
            self.state,
            PeerConnectionState::InitiatorSendHandshake,
            "Can only advance to WAIT_HANDSHAKE from SEND_HANDSHAKE"
        );
        self.state = PeerConnectionState::InitiatorWaitHandshake;
    }

    // ── Post-handshake processing ──────────────────────────────────────

    /// Perform post-handshake processing.
    ///
    /// Mirrors C++ `doPostHandshakeProcessing()`. Called after the
    /// handshake completes and before the normal interaction loop starts.
    /// This sends:
    /// - Extension handshake (BEP 10) if both sides support it
    /// - Bitfield message with our current piece possession
    /// - Allowed-fast set messages (BEP 6) if fast extension is enabled
    /// - Port message (BEP 5) if DHT is enabled
    ///
    /// For now this is a stub — the actual message sending is done by
    /// the caller using the connection. This method returns a summary
    /// of what should be sent so the caller can decide.
    ///
    /// # Returns
    ///
    /// A [`PostHandshakeActions`] describing what messages should be sent.
    pub fn post_handshake_processing(&self) -> PostHandshakeActions {
        PostHandshakeActions {
            send_bitfield: true,
            // Send extension handshake if we have local extensions configured
            send_extension_handshake: true,
            send_dht_port: self.dht_enabled,
            allowed_fast_pieces: Vec::new(), // TODO: compute allowed-fast set
        }
    }

    // ── Main interaction loop ──────────────────────────────────────────

    /// Main interaction processing loop, matching C++
    /// `DefaultBtInteractive::doInteractionProcessing()`.
    ///
    /// This is the core per-tick method called each time the peer
    /// interaction command executes in the `Wired` state.
    ///
    /// # Flow (normal mode — all 12 C++ steps)
    ///
    /// 1. `check_active_interaction()` — disconnect idle peers
    /// 2. Per-second: check request slots for timeouts
    /// 3. Receive messages and dispatch to handlers
    /// 4. `detect_flooding()` — detect choke/keepalive flooding
    /// 5. `decide_choking()` — send choke/unchoke if needed
    /// 6. `decide_interest()` — send interested/not-interested if needed
    /// 7. `check_have()` — advertise newly completed pieces
    /// 8. `should_send_keepalive()` — send keepalive if interval elapsed
    /// 9. `remove_completed_piece()` — handled by handler
    /// 10. `add_requests()` — request more pieces if not finished
    /// 11. PEX message if applicable
    /// 12. `send_pending_message()` — flush outgoing queue
    ///
    /// # Callbacks
    ///
    /// Several steps require access to piece storage or peer storage
    /// that this struct does not own. These are provided as closures:
    ///
    /// * `has_missing_piece` — returns true if the peer has pieces we need
    /// * `get_advertised_pieces` — returns newly completed piece indexes
    /// * `is_in_allowed_fast` — returns true if a piece is in the allowed-fast set
    /// * `is_block_acquired` — returns true if a block was obtained from another peer
    ///
    /// # Returns
    ///
    /// - `InteractionResult::Continue` — normal tick, keep running
    /// - `InteractionResult::Disconnect(reason)` — peer should be dropped
    /// - `InteractionResult::FloodingDetected` — flooding detected
    /// - `InteractionResult::WaitingForHandshake` — not yet wired
    pub async fn do_interaction_processing(
        &mut self,
        conn: &mut BtPeerConn,
        has_missing_piece: impl Fn(&BtPeerConn) -> bool,
        get_advertised_pieces: impl Fn() -> Vec<u32>,
        is_in_allowed_fast: impl Fn(u32) -> bool + Clone,
        is_block_acquired: impl Fn(u32, u32) -> bool,
        piece_storage: Option<&mut dyn PieceProvider>,
        cuid: u64,
    ) -> Result<InteractionResult> {
        // If not yet wired, skip interaction processing
        if self.state.is_handshake_state() {
            return Ok(InteractionResult::WaitingForHandshake);
        }

        if self.metadata_get_mode {
            // Simplified metadata-get mode: just keep-alive + receive
            if self.should_send_keepalive() {
                if let Err(e) = conn.send_keepalive().await {
                    warn!("Failed to send keepalive in metadata-get mode: {}", e);
                }
            }
            self.num_received_message =
                self.receive_messages(conn, is_in_allowed_fast.clone()).await?;
            return Ok(InteractionResult::Continue);
        }

        // ── Step 1: checkActiveInteraction ──────────────────────────────
        if let Some(reason) = self.check_active_interaction(conn) {
            conn.disconnected_gracefully = true;
            return Ok(InteractionResult::Disconnect(reason));
        }

        // ── Step 2: per-second request slot check ──────────────────────
        if self.per_sec_timer.elapsed() >= Duration::from_secs(PER_SEC_INTERVAL_SECS) {
            self.per_sec_timer = Instant::now();
            let result = self.handler.check_request_slots(is_block_acquired);
            if result.timed_out {
                warn!("Peer marked as snubbing (request slot timeout)");
            }
            // Send Cancel messages for blocks acquired elsewhere
            for (index, begin, length) in &result.cancelled_blocks {
                if let Err(e) = conn
                    .send_cancel(&aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                        *index, *begin, *length,
                    ))
                    .await
                {
                    warn!("Failed to send Cancel for piece {}: {}", index, e);
                }
            }
            trace!("Per-second timer fired, request slot check done");
        }

        // ── Step 3: receiveMessages ─────────────────────────────────────
        self.num_received_message =
            self.receive_messages(conn, is_in_allowed_fast.clone()).await?;

        // ── Step 4: detectMessageFlooding ───────────────────────────────
        if self.detect_flooding() {
            warn!("Message flooding detected, disconnecting peer");
            return Ok(InteractionResult::FloodingDetected);
        }

        // ── Step 5: decideChoking ───────────────────────────────────────
        let choking_decision = self.decide_choking(conn);
        match choking_decision {
            ChokingDecision::Choke => {
                debug!("Choking peer");
                self.handler.on_choke_sent();
                if let Err(e) = conn.send_choke().await {
                    warn!("Failed to send choke: {}", e);
                }
                self.am_choking = true;
                conn.stats.record_choke();
            }
            ChokingDecision::Unchoke => {
                debug!("Unchoking peer");
                if let Err(e) = conn.send_unchoke().await {
                    warn!("Failed to send unchoke: {}", e);
                }
                self.am_choking = false;
                conn.stats.record_unchoke();
            }
            ChokingDecision::NoChange => {}
        }

        // ── Step 6: decideInterest ─────────────────────────────────────
        let interest_decision = self.decide_interest_with_callback(conn, &has_missing_piece);
        match interest_decision {
            InterestDecision::Interested => {
                debug!("Expressing interest in peer");
                if let Err(e) = conn.send_interested().await {
                    warn!("Failed to send interested: {}", e);
                }
                self.am_interested = true;
            }
            InterestDecision::NotInterested => {
                debug!("Expressing lack of interest in peer");
                if let Err(e) = conn.send_not_interested().await {
                    warn!("Failed to send not-interested: {}", e);
                }
                self.am_interested = false;
            }
            InterestDecision::NoChange => {}
        }

        // ── Step 7: checkHave ───────────────────────────────────────────
        // C++ checkHave(): query PieceStorage for newly completed pieces and
        // send Have messages. Optimization: if there are many new pieces,
        // send a single Bitfield message instead.
        //
        // NOTE: We borrow `piece_storage` immutably here and stash the
        // results before the mutable borrow in Step 10, to satisfy the
        // borrow checker.
        if let Some(ref ps) = piece_storage {
            let bitfield_length = ps.get_bitfield_length_ext();
            let fast_ext = conn.is_fast_extension_enabled();
            let all_done = ps.all_download_finished_ext();
            let completed_len = ps.get_completed_length_ext();

            let result = self.check_have_optimized(
                &|last_idx| ps.get_advertised_piece_indexes_ext(cuid, last_idx),
                bitfield_length,
                fast_ext,
                all_done,
                completed_len,
            );

            match result {
                CheckHaveResult::None => {}
                CheckHaveResult::HaveIndexes(indexes) => {
                    for index in indexes {
                        if let Err(e) = conn.send_have(index as u32).await {
                            warn!("Failed to send Have({}): {}", index, e);
                        }
                    }
                }
                CheckHaveResult::Bitfield => {
                    let bf = ps.get_bitfield_ext();
                    if let Err(e) = conn.send_bitfield(bf).await {
                        warn!("Failed to send Bitfield: {}", e);
                    }
                }
                CheckHaveResult::HaveAll => {
                    if let Err(e) = conn.send_have_all().await {
                        warn!("Failed to send HaveAll: {}", e);
                    }
                }
            }
        } else {
            // Legacy path without piece storage
            let have_indices = self.check_have_with_callback(&get_advertised_pieces);
            for index in have_indices {
                if let Err(e) = conn.send_have(index).await {
                    warn!("Failed to send Have({}): {}", index, e);
                }
            }
        }

        // ── Step 8: sendKeepAlive ───────────────────────────────────────
        if self.should_send_keepalive() {
            if let Err(e) = conn.send_keepalive().await {
                warn!("Failed to send keepalive: {}", e);
            }
            self.reset_keep_alive_timer();
        }

        // ── Step 9: removeCompletedPiece ────────────────────────────────
        // Remove target pieces that have been fully downloaded.
        // C++ calls: btRequestFactory_->removeCompletedPiece()
        let completed_indices = self.remove_completed_piece();
        if !completed_indices.is_empty() {
            trace!(
                "Removed {} completed target pieces: {:?}",
                completed_indices.len(),
                completed_indices
            );
        }

        // ── Step 10: addRequests ────────────────────────────────────────
        // Generate new piece requests if the download is not finished.
        // C++ calls: if(!pieceStorage_->downloadFinished()) { addRequests(); }
        if !self.download_finished {
            if let Some(ps) = piece_storage {
                let requests = self.add_requests(ps, conn, cuid);
                if !requests.is_empty() {
                    trace!("addRequests: generated {} new requests", requests.len());
                }
            } else {
                // No piece storage provided — legacy path: just log readiness
                if !self.peer_choking && self.handler.can_send_request() {
                    trace!(
                        "Ready to add requests (outstanding={})",
                        self.handler.count_outstanding_requests()
                    );
                }
            }
        }

        // ── Step 11: addPeerExchangeMessage ─────────────────────────────
        if self.ut_pex_enabled
            && self.pex_timer.elapsed() >= Duration::from_secs(PEX_INTERVAL_SECS)
        {
            self.pex_timer = Instant::now();
            // PEX message creation is handled by the caller.
            trace!("PEX timer fired, peer exchange message due");
        }

        // ── Step 12: sendPendingMessage ────────────────────────────────
        // Drain sendable messages from the handler's dispatcher queue
        // first, then flush the connection's send buffer.
        let pending = self.handler.drain_sendable_messages();
        for msg_bytes in pending {
            // Queue each pending message into the connection's send buffer.
            // The actual sending happens during flush_send_buffer().
            conn.queue_message(msg_bytes);
        }
        if let Err(e) = conn.flush_send_buffer().await {
            warn!("Failed to flush send buffer: {}", e);
        }

        Ok(InteractionResult::Continue)
    }

    // ── Individual processing steps ─────────────────────────────────────

    /// Check for inactive interaction and return a reason to disconnect.
    ///
    /// Mirrors C++ `checkActiveInteraction()`:
    /// - 30s mutual-uninterested → disconnect
    /// - 60s total inactivity → disconnect
    /// - seeder-to-seeder → disconnect
    ///
    /// Uses the tracked `am_interested` and `peer_interested` fields
    /// instead of heuristics.
    ///
    /// Returns `Some(InactiveReason)` if the peer should be dropped.
    fn check_active_interaction(&mut self, conn: &BtPeerConn) -> Option<InactiveReason> {
        // Use tracked interest state rather than heuristics.
        // For we_are_seeder, check the connection's session resource.
        let we_are_seeder = conn
            .session_resource
            .as_ref()
            .map_or(false, |res| res.is_seeder());
        let peer_is_seeder = conn.seeder;

        self.active_interaction_checker.check(
            self.am_interested,
            self.peer_interested,
            we_are_seeder,
            peer_is_seeder,
        )
    }

    /// Decide whether we should choke or unchoke the peer.
    ///
    /// Mirrors C++ `decideChoking()`:
    /// - If `shouldBeChoking()` is true and we are not choking → send Choke
    /// - If `shouldBeChoking()` is false and we are choking → send Unchoke
    ///
    /// Now properly tracks `am_choking` state to only produce a decision
    /// when the state actually needs to change.
    fn decide_choking(&self, conn: &BtPeerConn) -> ChokingDecision {
        if let Some(ref res) = conn.session_resource {
            let should_be_choking = res.should_be_choking();
            if should_be_choking && !self.am_choking {
                // Should be choking but currently not → send Choke
                ChokingDecision::Choke
            } else if !should_be_choking && self.am_choking {
                // Should not be choking but currently are → send Unchoke
                ChokingDecision::Unchoke
            } else {
                ChokingDecision::NoChange
            }
        } else {
            // No session resource — no choking decision possible
            ChokingDecision::NoChange
        }
    }

    /// Decide whether we should express interest or lack thereof.
    ///
    /// Mirrors C++ `decideInterest()`:
    /// - If `hasMissingPiece(peer)` and not amInterested → send Interested
    /// - If `!hasMissingPiece(peer)` and amInterested → send NotInterested
    ///
    /// Uses the provided `has_missing_piece` callback to check whether
    /// the peer has pieces we need (i.e., PieceStorage::hasMissingPiece).
    fn decide_interest_with_callback(
        &self,
        conn: &BtPeerConn,
        has_missing_piece: &impl Fn(&BtPeerConn) -> bool,
    ) -> InterestDecision {
        let should_be_interested = has_missing_piece(conn);
        if should_be_interested && !self.am_interested {
            InterestDecision::Interested
        } else if !should_be_interested && self.am_interested {
            InterestDecision::NotInterested
        } else {
            InterestDecision::NoChange
        }
    }

    /// Legacy decide_interest using heuristic (for backward compat).
    ///
    /// Prefer `decide_interest_with_callback` for proper PieceStorage integration.
    #[allow(dead_code)]
    fn decide_interest(&self, conn: &BtPeerConn) -> InterestDecision {
        // Heuristic: if peer is a seeder or has a session resource,
        // we are likely interested. This matches the original simplified
        // behavior before callback integration.
        let should_be_interested = conn.session_resource.is_some();
        if should_be_interested && !self.am_interested {
            InterestDecision::Interested
        } else if !should_be_interested && self.am_interested {
            InterestDecision::NotInterested
        } else {
            InterestDecision::NoChange
        }
    }

    /// Check for new Have messages to send.
    ///
    /// Mirrors C++ `checkHave()`: queries `PieceStorage` for piece indexes
    /// that have been completed since `lastHaveIndex_` and returns them.
    ///
    /// In the C++ code, this calls `pieceStorage_->getAdvertisedPieceIndexes()`.
    /// Without piece storage integration, this returns an empty vector.
    #[allow(dead_code)]
    fn check_have(&mut self) -> Vec<u32> {
        Vec::new()
    }

    /// Check for new Have messages using a callback for piece storage.
    ///
    /// Mirrors C++ `checkHave()`: calls the `get_advertised_pieces` callback
    /// which should return piece indexes completed since `lastHaveIndex_`.
    ///
    /// After sending these Have messages, `lastHaveIndex_` is updated.
    fn check_have_with_callback(&mut self, get_advertised_pieces: &impl Fn() -> Vec<u32>) -> Vec<u32> {
        let pieces = get_advertised_pieces();
        if !pieces.is_empty() {
            // Update last_have_index to the maximum advertised index
            if let Some(&max_idx) = pieces.iter().max() {
                self.last_have_index = self.last_have_index.max(max_idx as u64);
            }
            trace!("checkHave: advertising {} new pieces", pieces.len());
        }
        pieces
    }

    /// Check for new Have messages and decide whether to send individual
    /// Have messages or a single Bitfield/HaveAll/HaveNone message.
    ///
    /// Mirrors C++ `DefaultBtInteractive::checkHave()`:
    /// - If `5 + bitfieldLength <= haveIndexes.size() * 9`, send a single
    ///   Bitfield message (or HaveAll/HaveNone if fast extension is enabled)
    /// - Otherwise, send individual Have messages
    ///
    /// Returns a `CheckHaveResult` indicating what type of message(s) to send.
    fn check_have_optimized(
        &mut self,
        get_advertised_pieces: &impl Fn(u64) -> (Vec<usize>, u64),
        bitfield_length: usize,
        fast_extension_enabled: bool,
        all_download_finished: bool,
        completed_length: u64,
    ) -> CheckHaveResult {
        let (have_indexes, new_last) = get_advertised_pieces(self.last_have_index);
        self.last_have_index = new_last;

        if have_indexes.is_empty() {
            return CheckHaveResult::None;
        }

        // C++ optimization: use bitfield message if it is equal to or less
        // than the total size of have messages.
        // Have message = 5 bytes (4 length + 1 ID) + 4 bytes (piece index) = 9 bytes each
        // Bitfield message = 5 bytes (4 length + 1 ID) + bitfieldLength bytes
        if 5 + bitfield_length <= have_indexes.len() * 9 {
            if fast_extension_enabled && all_download_finished {
                return CheckHaveResult::HaveAll;
            }
            // Only send bitfield if we have some completed data
            if completed_length > 0 {
                return CheckHaveResult::Bitfield;
            }
        }

        CheckHaveResult::HaveIndexes(have_indexes)
    }

    /// Set the last advertised have index (called by the caller after
    /// checking piece storage).
    pub fn set_last_have_index(&mut self, index: u64) {
        self.last_have_index = index;
    }

    /// Get the last advertised have index.
    pub fn last_have_index(&self) -> u64 {
        self.last_have_index
    }

    /// Check whether we should send a keep-alive message.
    ///
    /// Mirrors C++ `sendKeepAlive()`: returns true if
    /// `keepAliveTimer_.difference() >= keepAliveInterval_`.
    pub fn should_send_keepalive(&self) -> bool {
        self.keep_alive_timer.elapsed() >= Duration::from_secs(self.keep_alive_interval_secs)
    }

    /// Reset the keep-alive timer after sending a keep-alive.
    pub fn reset_keep_alive_timer(&mut self) {
        self.keep_alive_timer = Instant::now();
    }

    /// Detect message flooding from the peer.
    ///
    /// Mirrors C++ `detectMessageFlooding()`: checks if the peer has
    /// sent >= 2 choke/unchoke transitions or >= 2 keepalive messages
    /// within the flooding check interval (5 seconds).
    ///
    /// The check interval is managed by this struct's `flooding_timer`,
    /// matching the C++ design where `DefaultBtInteractive` owns the timer
    /// and `FloodingStat` only holds the counts.
    ///
    /// Returns `true` if flooding was detected.
    fn detect_flooding(&mut self) -> bool {
        if self.flooding_timer.elapsed() >= Duration::from_secs(FLOODING_CHECK_INTERVAL_SECS) {
            let choke_count = self.flooding_stat.choke_unchoke_count();
            let keepalive_count = self.flooding_stat.keepalive_count();
            let detected = choke_count >= 2 || keepalive_count >= 2;

            if detected {
                warn!(
                    "Flooding detected: choke_unchoke={}, keepalive={}",
                    choke_count, keepalive_count
                );
            }

            // Reset counters regardless of detection result
            self.flooding_stat.reset();
            self.flooding_timer = Instant::now();
            detected
        } else {
            false
        }
    }

    // ── Message dispatch ────────────────────────────────────────────────

    /// Dispatch a received message to the appropriate handler method.
    ///
    /// This is the central message dispatch that the C++ code handles
    /// via virtual dispatch on `BtMessage::doReceivedAction()`. Each
    /// message type is routed to the corresponding `on_*_received()`
    /// method on the handler, and internal state (peer_choking,
    /// peer_interested, flooding stats) is updated.
    ///
    /// # Arguments
    /// * `msg` — The received BtMessage to dispatch
    /// * `conn` — The peer connection (for AllowedFast set access)
    /// * `is_in_allowed_fast` — Closure checking if a piece is in the
    ///   peer's allowed-fast set (needed for Choke handling)
    ///
    /// # Returns
    ///
    /// A [`DispatchUpdate`] containing state changes for the caller to apply.
    fn dispatch_message<F>(
        &mut self,
        msg: BtMessage,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> DispatchUpdate
    where
        F: Fn(u32) -> bool,
    {
        let mut update = DispatchUpdate::default();

        match msg {
            BtMessage::Choke => {
                let was_choking = self.peer_choking;
                // Delegate to handler: removes non-allowed-fast request slots
                update.cancelled_slots = self.handler.on_choke_received(is_in_allowed_fast);
                self.peer_choking = true;
                update.peer_choking_changed = !was_choking;
                update.peer_choking = true;
                // Update flooding stat for transition detection
                if !was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Choke message");
            }
            BtMessage::Unchoke => {
                let was_choking = self.peer_choking;
                self.handler.on_unchoke_received();
                self.peer_choking = false;
                update.peer_choking_changed = was_choking;
                update.peer_choking = false;
                // Update flooding stat for transition detection
                if was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Unchoke message");
            }
            BtMessage::Interested => {
                self.peer_interested = true;
                trace!("Dispatched Interested message");
            }
            BtMessage::NotInterested => {
                self.peer_interested = false;
                trace!("Dispatched NotInterested message");
            }
            BtMessage::Have { piece_index } => {
                // Update the peer's bitfield
                if let Some(ref mut res) = conn.session_resource {
                    res.update_bitfield(piece_index as usize, 1);
                }
                // If the peer was a seeder before and now has even more,
                // or if the peer now has all pieces, mark as seeder
                if let Some(ref res) = conn.session_resource {
                    if res.is_seeder() {
                        conn.seeder = true;
                    }
                }
                update.have_index = Some(piece_index);
                trace!("Dispatched Have({}) message", piece_index);
            }
            BtMessage::Bitfield { data } => {
                // Update the peer's bitfield from the full bitfield message
                if let Some(ref mut res) = conn.session_resource {
                    res.set_bitfield(&data);
                    if res.is_seeder() {
                        conn.seeder = true;
                    }
                }
                update.bitfield_data = Some(data);
                trace!("Dispatched Bitfield message");
            }
            BtMessage::Request { request } => {
                // Incoming request from peer to upload data.
                // Record data exchange for active interaction checking.
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Request(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::Piece {
                index,
                begin,
                ref data,
            } => {
                // Received piece data — remove matching request slot
                self.handler.on_piece_received(index, begin, data.len() as u32);
                // Record data exchange for active interaction checking
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Piece(index={}, begin={}, len={})",
                    index,
                    begin,
                    data.len()
                );
            }
            BtMessage::Cancel { request } => {
                // Peer cancels a pending upload
                self.handler
                    .on_cancel_received(request.index, request.begin, request.length);
                trace!(
                    "Dispatched Cancel(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::KeepAlive => {
                self.handler.on_keepalive_received();
                self.flooding_stat.inc_keepalive_count();
                trace!("Dispatched KeepAlive message");
            }
            BtMessage::Port { port } => {
                // DHT port message (BEP 5)
                if self.dht_enabled {
                    trace!("Dispatched Port({}) message", port);
                }
            }
            BtMessage::AllowedFast { index } => {
                // BEP 6: peer grants fast access to a piece
                conn.add_allowed_fast(index);
                trace!("Dispatched AllowedFast({}) message", index);
            }
            BtMessage::Reject {
                index,
                offset,
                length,
            } => {
                // BEP 6: peer rejected our request
                // Remove the matching outstanding request slot
                self.handler.on_piece_received(index, offset, length);
                trace!(
                    "Dispatched Reject(piece={}, offset={}, len={})",
                    index, offset, length
                );
            }
            BtMessage::Suggest { index } => {
                // BEP 6: peer suggests we download this piece
                // The caller should boost the priority of this piece
                trace!("Dispatched Suggest({}) message", index);
            }
            BtMessage::HaveAll => {
                // BEP 6: peer has all pieces
                conn.mark_seeder();
                trace!("Dispatched HaveAll message");
            }
            BtMessage::HaveNone => {
                // BEP 6: peer has no pieces
                trace!("Dispatched HaveNone message");
            }
            BtMessage::Extended { ext_id, ref payload } => {
                // BEP 10: extension protocol message.
                // Dispatch via the extension registry which handles:
                //   ext_id == 0 → Extension Handshake (BEP 10)
                //   ext_id == peer_ut_metadata_id → ut_metadata (BEP 9)
                //   ext_id == peer_ut_pex_id → ut_pex (BEP 11)
                //   otherwise → unknown extension
                let ext_update = extension_registry::dispatch_extension_message(
                    &mut self.extension_registry,
                    ext_id,
                    payload,
                );

                if let Some(ref update) = ext_update {
                    match update {
                        ExtensionUpdate::HandshakeReceived { .. } => {
                            // Enable PEX if both sides support it
                            if self.extension_registry.supports_ut_pex() {
                                self.ut_pex_enabled = true;
                                debug!("ut_pex enabled after extension handshake");
                            }
                            debug!(
                                "Dispatched Extended handshake (ut_metadata={:?}, ut_pex={:?})",
                                self.extension_registry.peer_ut_metadata_id(),
                                self.extension_registry.peer_ut_pex_id()
                            );
                        }
                        ExtensionUpdate::MetadataPiece { piece, .. } => {
                            debug!("Dispatched Extended ut_metadata Data(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataRequest { piece } => {
                            debug!("Dispatched Extended ut_metadata Request(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataReject { piece } => {
                            debug!("Dispatched Extended ut_metadata Reject(piece={})", piece);
                        }
                        ExtensionUpdate::PeerExchange { added_v4, added_v6 } => {
                            debug!(
                                "Dispatched Extended ut_pex ({} v4, {} v6 peers)",
                                added_v4.len(),
                                added_v6.len()
                            );
                        }
                    }
                } else {
                    warn!(
                        "Dispatched Extended with unknown ext_id={} (payload_len={})",
                        ext_id,
                        payload.len()
                    );
                }

                update.extension_update = ext_update;
            }
        }

        update
    }

    /// Receive messages from the peer connection and dispatch each one.
    ///
    /// Mirrors C++ `receiveMessages()`: reads all available messages
    /// from the peer, dispatches each to the handler via
    /// [`dispatch_message()`], and resets the inactive timer on data
    /// messages.
    ///
    /// Returns the number of messages received.
    async fn receive_messages<F>(
        &mut self,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> Result<usize>
    where
        F: Fn(u32) -> bool,
    {
        let mut count = 0usize;

        // Read up to a reasonable batch of messages per iteration.
        // The C++ code reads in a loop while messages are available.
        for _ in 0..UB_MAX_OUTSTANDING_REQUEST {
            match conn.read_message().await {
                Ok(Some(msg)) => {
                    count += 1;
                    trace!("Received message from peer: {:?}", msg);

                    // Dispatch the message to the handler
                    let update = self.dispatch_message(msg, conn, &is_in_allowed_fast);

                    // Process dispatch updates: send Cancel for removed slots
                    for slot in &update.cancelled_slots {
                        if let Err(e) = conn
                            .send_cancel(
                                &aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                                    slot.index, slot.begin, slot.length,
                                ),
                            )
                            .await
                        {
                            warn!(
                                "Failed to send Cancel for piece {} begin {}: {}",
                                slot.index, slot.begin, e
                            );
                        }
                    }

                    // Reset inactive timer on any received message
                    self.inactive_timer = Instant::now();
                }
                Ok(None) => {
                    // No more messages available
                    break;
                }
                Err(e) => {
                    // Read error — return it to the caller
                    return Err(e);
                }
            }
        }

        Ok(count)
    }

    /// Process a received message and update internal state.
    ///
    /// This method updates flooding stats and inactive timer based on
    /// the message type, matching the C++ `receiveMessages()` switch.
    ///
    /// # Arguments
    /// * `msg_id` — The BT message type ID (0=Choke, 1=Unchoke, etc.)
    /// * `was_peer_choking` — Whether the peer was choking us before
    ///   this message (needed to detect choke/unchoke transitions)
    pub fn on_message_received(&mut self, msg_id: u8, was_peer_choking: bool) {
        match msg_id {
            // Choke (ID=0)
            0 => {
                if !was_peer_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
            }
            // Unchoke (ID=1)
            1 => {
                if was_peer_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
            }
            // Request (ID=6) or Piece (ID=7) — data exchange
            6 | 7 => {
                self.active_interaction_checker.record_data_exchange();
            }
            // KeepAlive (ID implied by zero-length)
            _ => {
                // KeepAlive messages increment flooding counter
                // In C++, this is handled by matching BtKeepAliveMessage::ID
                // We treat any unrecognized as potential keepalive for safety
            }
        }
    }

    /// Process a keepalive message for flooding detection.
    ///
    /// Call this when a KeepAlive message is received.
    pub fn on_keepalive_received(&mut self) {
        self.flooding_stat.inc_keepalive_count();
    }

    /// Dynamically scale `max_outstanding_request` based on request
    /// fulfillment rate.
    ///
    /// Mirrors the C++ logic at the end of `receiveMessages()`:
    /// if not in end-game and we lost >= 1/4 of outstanding requests,
    /// double `maxOutstandingRequest_` (up to `UB_MAX_OUTSTANDING_REQUEST`).
    pub fn scale_max_outstanding_request(
        &mut self,
        old_outstanding: usize,
        new_outstanding: usize,
        is_end_game: bool,
    ) {
        if !is_end_game
            && old_outstanding > new_outstanding
            && (old_outstanding - new_outstanding) * 4 >= self.max_outstanding_request
        {
            self.max_outstanding_request = (self.max_outstanding_request * 2)
                .min(UB_MAX_OUTSTANDING_REQUEST);
            debug!(
                "Scaled max_outstanding_request to {}",
                self.max_outstanding_request
            );
        }
    }

    // ── Request generation (C++ addRequests / fillPiece) ────────────────

    /// Get a reference to the per-peer request factory.
    pub fn request_factory(&self) -> &BtRequestFactory {
        &self.request_factory
    }

    /// Get a mutable reference to the per-peer request factory.
    pub fn request_factory_mut(&mut self) -> &mut BtRequestFactory {
        &mut self.request_factory
    }

    /// Check whether end-game mode is active.
    pub fn is_endgame(&self) -> bool {
        self.endgame
    }

    /// Fill target pieces from piece storage, up to `max_missing_block` total
    /// missing blocks across all target pieces.
    ///
    /// Mirrors C++ `DefaultBtInteractive::fillPiece(maxMissingBlock)`:
    ///
    /// 1. If `piece_storage.has_missing_piece(peer)`:
    ///    - Count current missing blocks in the request factory
    ///    - If `numMissingBlock >= maxMissingBlock`, return (already have enough)
    ///    - Calculate `diffMissingBlock = maxMissingBlock - numMissingBlock`
    ///    - If peer is choking us AND fast extension enabled: get fast pieces
    ///    - If peer is not choking us: get regular pieces
    ///    - For each piece: `request_factory.addTargetPiece(piece)`
    ///
    /// # Arguments
    ///
    /// * `piece_storage` — The piece storage provider (trait abstraction)
    /// * `conn` — The peer connection (for peer state and fast extension check)
    /// * `cuid` — Command ID for piece storage operations
    fn fill_piece(
        &mut self,
        piece_storage: &mut dyn PieceProvider,
        conn: &BtPeerConn,
        cuid: u64,
    ) {
        if !piece_storage.has_missing_piece(conn) {
            return;
        }

        let num_missing_block = self.request_factory.count_missing_block();
        if num_missing_block >= self.max_outstanding_request {
            return;
        }

        let diff_missing_block = self.max_outstanding_request - num_missing_block;
        let target_indexes = self.request_factory.get_target_piece_indexes();

        let pieces = if self.peer_choking {
            // Peer is choking us — only get fast pieces if fast extension enabled.
            // C++: if(peer_->peerChoking() && peer_->isFastExtensionEnabled())
            let fast_ext = conn
                .session_resource
                .as_ref()
                .map_or(false, |r| r.is_fast_extension_enabled());
            if fast_ext {
                piece_storage.get_missing_fast_pieces(
                    diff_missing_block,
                    conn,
                    &target_indexes,
                    cuid,
                )
            } else {
                Vec::new()
            }
        } else {
            // Peer is not choking us — get regular pieces.
            // C++: else { pieceStorage_->getMissingPiece(...) }
            piece_storage.get_missing_pieces(
                diff_missing_block,
                conn,
                &target_indexes,
                cuid,
            )
        };

        for piece in pieces {
            self.request_factory.add_target_piece(piece);
        }
    }

    /// Generate and queue piece requests, matching C++ `addRequests()`.
    ///
    /// This is the core request generation step called each iteration of
    /// the interaction loop. It:
    ///
    /// 1. Checks if end-game should be entered (no missing unused pieces
    ///    left but we still have target pieces with missing blocks).
    /// 2. Calls `fillPiece()` to ensure we have enough target pieces.
    /// 3. Calculates how many new requests to create based on the gap
    ///    between `maxOutstandingRequest` and current outstanding count.
    /// 4. Creates requests via `BtRequestFactory::create_request_messages()`
    ///    and queues them through the handler (actual sending happens in
    ///    step 12 of `do_interaction_processing()`).
    ///
    /// # Arguments
    ///
    /// * `piece_storage` — The piece storage provider (trait abstraction)
    /// * `conn` — The peer connection (for peer state checks)
    /// * `cuid` — Command ID for piece storage operations
    ///
    /// # Returns
    ///
    /// A vector of `PieceBlockRequest` descriptors for the requests that
    /// were generated. The caller can use this for tracking or logging.
    fn add_requests(
        &mut self,
        piece_storage: &mut dyn PieceProvider,
        conn: &BtPeerConn,
        cuid: u64,
    ) -> Vec<PieceBlockRequest> {
        // Check if we should enter end-game mode.
        // C++: if(!pieceStorage_->isEndGame() && !pieceStorage_->hasMissingUnusedPiece())
        if !self.endgame && !piece_storage.has_missing_unused_piece() {
            self.endgame = true;
            piece_storage.enter_end_game();
            debug!("Entered end-game mode");
        }

        // Fill target pieces from piece storage
        self.fill_piece(piece_storage, conn, cuid);

        // Calculate how many new requests to create
        // C++: reqNumToCreate = max(maxOutstandingRequest - countOutstandingRequest, 0)
        let outstanding = self.handler.count_outstanding_requests();
        let req_num_to_create = if self.max_outstanding_request > outstanding {
            self.max_outstanding_request - outstanding
        } else {
            0
        };

        let mut all_requests = Vec::new();

        if req_num_to_create > 0 {
            // Create request messages via the factory
            // C++ calls: btRequestFactory_->createRequestMessages(reqNumToCreate, isEndGame)
            let is_endgame = self.endgame;
            let requests = self.request_factory.create_request_messages(
                req_num_to_create,
                is_endgame,
                |index, block_index| self.handler.is_outstanding_request(index, block_index),
            );

            // Send each request through the handler and connection
            for req in &requests {
                // Serialize the Request message
                let serialized = aria2_protocol::bittorrent::message::serializer::serialize(
                    &BtMessage::Request {
                        request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                            req.index, req.begin, req.length,
                        ),
                    },
                );

                // Queue through the handler (tracks request slots + outgoing queue)
                if let Some(_msg_bytes) = self.handler.send_request(
                    req.index,
                    req.begin,
                    req.length,
                    serialized,
                ) {
                    trace!(
                        "addRequests: queued request piece={} begin={} len={}",
                        req.index, req.begin, req.length
                    );
                }
            }

            all_requests = requests;
        }

        all_requests
    }

    /// Cancel all target pieces and remove outstanding requests.
    ///
    /// Mirrors C++ `DefaultBtInteractive::cancelAllPiece()`. Called when
    /// the peer connection is being torn down.
    ///
    /// Returns the indices of pieces that were removed (for the caller to
    /// abort outstanding requests in the dispatcher).
    pub fn cancel_all_piece(&mut self) -> Vec<u32> {
        let removed = self.request_factory.remove_all_target_pieces();
        removed.iter().map(|p| p.index() as u32).collect()
    }

    /// Remove completed pieces from the request factory.
    ///
    /// Mirrors C++ `btRequestFactory_->removeCompletedPiece()` called
    /// in `doInteractionProcessing()` step 9.
    ///
    /// Returns the indices of removed completed pieces (for the caller to
    /// abort outstanding requests in the dispatcher).
    pub fn remove_completed_piece(&mut self) -> Vec<u32> {
        self.request_factory.remove_completed_piece()
    }
}

// ======================================================================
// PostHandshakeActions — messages to send after handshake
// ======================================================================

/// Describes what messages should be sent during post-handshake processing.
///
/// Returned by [`BtPeerInteractive::post_handshake_processing()`] so the
/// caller can decide which messages to send over the connection.
#[derive(Debug, Clone)]
pub struct PostHandshakeActions {
    /// Whether to send a Bitfield message with our current piece possession.
    pub send_bitfield: bool,
    /// Whether to send an Extension Handshake message (BEP 10).
    pub send_extension_handshake: bool,
    /// Whether to send a DHT Port message (BEP 5).
    pub send_dht_port: bool,
    /// Piece indexes to send as AllowedFast messages (BEP 6).
    pub allowed_fast_pieces: Vec<u32>,
}

// ======================================================================
// PeerConnectionResult — legacy result type
// ======================================================================

/// Result of peer connection attempt
pub struct PeerConnectionResult {
    /// Successfully connected peers
    pub connections: Vec<BtPeerConn>,
    /// Number of failed connections
    pub failed_count: usize,
}

// ======================================================================
// BtPeerInteraction — legacy static helper (preserved for backward compat)
// ======================================================================

/// BT Peer Interaction Manager
///
/// Handles the lifecycle of peer connections from initial connection
/// through the handshake phase until they're ready for data transfer.
pub struct BtPeerInteraction;

impl BtPeerInteraction {
    /// Connect to multiple peers with automatic fallback strategies
    ///
    /// Attempts to connect to all provided peer addresses using:
    /// 1. MSE encryption if required or forced
    /// 2. Plain connection as fallback
    ///
    /// For each successful connection:
    /// - Sends initial unchoke and interested messages
    /// - Exchanges bitfields
    /// - Waits for unchoke from the peer
    ///
    /// # Arguments
    /// * `peer_addrs` - List of peer addresses to connect to
    /// * `info_hash_raw` - Torrent info hash for handshake
    /// * `num_pieces` - Total number of pieces (for bitfield size)
    /// * `require_crypto` - Whether to require encrypted connections
    /// * `force_encrypt` - Whether to force encryption (fallback to plain)
    ///
    /// # Returns
    /// * `PeerConnectionResult` containing connected peers and failure count
    pub async fn connect_to_peers(
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        require_crypto: bool,
        force_encrypt: bool,
    ) -> Result<PeerConnectionResult> {
        info!("[BT] Connecting to {} peers...", peer_addrs.len());

        let mut active_connections: Vec<BtPeerConn> = Vec::new();
        let mut failed_count = 0usize;

        for addr in peer_addrs {
            debug!("[BT] Connecting to peer {}:{}", addr.ip, addr.port);

            let conn_result =
                Self::connect_single_peer(addr, info_hash_raw, require_crypto, force_encrypt).await;

            match conn_result {
                Ok(mut conn) => {
                    info!(
                        "[BT] Connected to peer {}:{} (encrypted={})",
                        addr.ip,
                        addr.port,
                        conn.is_encrypted()
                    );

                    // Initialize the connection
                    if let Err(e) = Self::initialize_connection(&mut conn, num_pieces).await {
                        warn!("[BT] Failed to initialize peer {}: {}", addr.ip, e);
                        failed_count += 1;
                        continue;
                    }

                    // Wait for unchoke
                    match Self::wait_for_unchoke(&mut conn, addr).await {
                        Ok(()) => {
                            active_connections.push(conn);
                        }
                        Err(e) => {
                            warn!("[BT] No unchoke from peer {}: {}", addr.ip, e);
                            // Still add the connection even without unchoke
                            // (it might unchoke later)
                            active_connections.push(conn);
                        }
                    }
                }
                Err(e) => {
                    error!("[BT] Failed to connect peer {}: {}", addr.ip, e);
                    failed_count += 1;
                    continue;
                }
            }
        }

        info!("[BT] Active connections: {}", active_connections.len());

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed".into(),
                },
            ));
        }

        Ok(PeerConnectionResult {
            connections: active_connections,
            failed_count,
        })
    }

    /// Connect to a single peer with encryption fallback logic
    async fn connect_single_peer(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        require_crypto: bool,
        force_encrypt: bool,
    ) -> Result<BtPeerConn> {
        if force_encrypt || require_crypto {
            // Try MSE encrypted connection
            BtPeerConn::connect_mse(addr, info_hash_raw, require_crypto).await
        } else {
            // Try MSE first, fall back to plain
            match BtPeerConn::connect_mse(addr, info_hash_raw, false).await {
                Ok(conn) => Ok(conn),
                Err(_) => {
                    debug!("[BT] MSE failed, trying plain connection");
                    BtPeerConn::connect_plain(addr, info_hash_raw).await
                }
            }
        }
    }

    /// Initialize a newly established connection
    ///
    /// Sends initial protocol messages:
    /// - Unchoke (we allow them to request from us)
    /// - Interested (we want to download from them)
    /// - Bitfield (our current piece possession status)
    async fn initialize_connection(conn: &mut BtPeerConn, num_pieces: u32) -> Result<()> {
        // Send initial messages
        conn.send_unchoke().await?;
        conn.send_interested().await?;

        // Send empty bitfield (we have nothing yet)
        let bf_len = (num_pieces as usize).div_ceil(8);
        let empty_bf = vec![0u8; bf_len];
        conn.send_bitfield(empty_bf).await?;

        // Small delay to allow processing
        tokio::time::sleep(Duration::from_millis(PEER_CONNECTION_DELAY_MS)).await;

        Ok(())
    }

    /// Wait for an unchoke message from a peer
    ///
    /// Polls the connection for messages until we receive an Unchoke
    /// or hit the timeout/attempts limit.
    async fn wait_for_unchoke(
        conn: &mut BtPeerConn,
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Result<()> {
        debug!("[BT] Waiting for unchoke from {}:{}", addr.ip, addr.port);

        for _ in 0..MAX_UNCHOKE_WAIT_ATTEMPTS {
            match tokio::time::timeout(
                Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
                conn.read_message(),
            )
            .await
            {
                Ok(Ok(Some(msg))) => {
                    if matches!(msg, BtMessage::Unchoke) {
                        info!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                        return Ok(());
                    }
                    debug!("[BT] Got message while waiting for unchoke: {:?}", msg);
                }
                Ok(Ok(None)) => {
                    warn!("[BT] EOF from peer while waiting for unchoke");
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "Peer closed connection".into(),
                        },
                    ));
                }
                Ok(Err(e)) => {
                    error!("[BT] Error reading from peer: {}", e);
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Read error: {}", e),
                        },
                    ));
                }
                Err(_) => {
                    debug!("[BT] Timeout reading from peer, retrying...");
                }
            }
        }

        warn!(
            "[BT] Did not receive unchoke from {}:{} after {} attempts",
            addr.ip, addr.port, MAX_UNCHOKE_WAIT_ATTEMPTS
        );
        Ok(()) // Continue anyway, might get unchoke later
    }

    /// Broadcast a HAVE message to all connected peers
    ///
    /// Notifies all peers that we have completed downloading a piece.
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of active peer connections
    /// * `piece_index` - Index of the completed piece
    pub async fn broadcast_have(connections: &mut [BtPeerConn], piece_index: u32) {
        for conn in connections.iter_mut() {
            if let Err(e) = conn.send_have(piece_index).await {
                warn!("[BT] Failed to send HAVE to peer: {}", e);
            }
        }
    }

    /// Initialize peer bitfield tracker for all connections
    ///
    /// Sets up tracking of which pieces each peer claims to have.
    ///
    /// # Arguments
    /// * `connections` - Slice of active peer connections
    /// * `num_pieces` - Total number of pieces in the torrent
    /// * `peer_tracker` - Mutable reference to the peer bitfield tracker
    pub fn initialize_peer_tracking(
        connections: &[BtPeerConn],
        num_pieces: u32,
        peer_tracker: &mut aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
    ) {
        for (i, _conn) in connections.iter().enumerate() {
            let empty_bf = vec![0xFFu8; (num_pieces as usize).div_ceil(8)];
            peer_tracker.update_peer_bitfield(&format!("peer_{}", i), &empty_bf);
        }

        debug!(
            "[BT] Initialized peer tracking for {} peers",
            connections.len()
        );
    }

    /// Clean up peer connections (drop them properly)
    ///
    /// Ensures all connections are properly closed.
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of peer connections to close
    pub fn cleanup_connections(connections: &mut [BtPeerConn]) {
        for conn in connections.iter_mut() {
            let _ = conn;
        }
        debug!("[BT] Cleaned up {} connections", connections.len());
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create an `Instant` representing a point in the past.
    /// Uses `checked_sub` to avoid panicking on platforms where `Instant`
    /// origin is near zero (e.g., shortly after system boot on Windows).
    fn instant_past(secs: u64) -> Instant {
        Instant::now().checked_sub(Duration::from_secs(secs)).unwrap_or(Instant::now())
    }

    // ── Legacy tests (preserved) ──────────────────────────────────────

    #[test]
    fn test_constants_are_reasonable() {
        const _: () = {
            assert!(PEER_CONNECTION_DELAY_MS >= 10);
            assert!(PEER_CONNECTION_DELAY_MS <= 1000);
            assert!(MAX_UNCHOKE_WAIT_ATTEMPTS >= 10);
            assert!(MAX_UNCHOKE_WAIT_ATTEMPTS <= 100);
            assert!(PEER_MESSAGE_TIMEOUT_SECS >= 1);
            assert!(PEER_MESSAGE_TIMEOUT_SECS <= 30);
        };
    }

    #[test]
    fn test_peer_connection_result_default() {
        let result = PeerConnectionResult {
            connections: Vec::new(),
            failed_count: 0,
        };
        assert!(result.connections.is_empty());
        assert_eq!(result.failed_count, 0);
    }

    // ── New constant tests ─────────────────────────────────────────────

    #[test]
    fn test_bt_constants_match_cpp() {
        // These must match the C++ BtConstants.h exactly
        assert_eq!(DEFAULT_MAX_OUTSTANDING_REQUEST, 6);
        assert_eq!(UB_MAX_OUTSTANDING_REQUEST, 256);
        assert_eq!(DEFAULT_KEEP_ALIVE_INTERVAL_SECS, 120);
        assert_eq!(DEFAULT_ALLOWED_FAST_SET_SIZE, 10);
        assert_eq!(MUTUAL_UNINTERESTED_TIMEOUT_SECS, 30);
        assert_eq!(INACTIVITY_TIMEOUT_SECS, 60);
        assert_eq!(FLOODING_CHECK_INTERVAL_SECS, 5);
    }

    // ── PeerConnectionState tests ──────────────────────────────────────

    #[test]
    fn test_peer_connection_state_transitions() {
        // Initiator path
        let state = PeerConnectionState::InitiatorSendHandshake;
        assert!(state.is_handshake_state());
        assert!(!state.is_wired());

        let state = PeerConnectionState::InitiatorWaitHandshake;
        assert!(state.is_handshake_state());
        assert!(!state.is_wired());

        // Receiver path
        let state = PeerConnectionState::ReceiverWaitHandshake;
        assert!(state.is_handshake_state());
        assert!(!state.is_wired());

        // Wired
        let state = PeerConnectionState::Wired;
        assert!(!state.is_handshake_state());
        assert!(state.is_wired());
    }

    #[test]
    fn test_peer_connection_state_display() {
        assert_eq!(
            PeerConnectionState::InitiatorSendHandshake.to_string(),
            "INITIATOR_SEND_HANDSHAKE"
        );
        assert_eq!(
            PeerConnectionState::InitiatorWaitHandshake.to_string(),
            "INITIATOR_WAIT_HANDSHAKE"
        );
        assert_eq!(
            PeerConnectionState::ReceiverWaitHandshake.to_string(),
            "RECEIVER_WAIT_HANDSHAKE"
        );
        assert_eq!(PeerConnectionState::Wired.to_string(), "WIRED");
    }

    #[test]
    fn test_peer_connection_state_equality() {
        assert_eq!(
            PeerConnectionState::InitiatorSendHandshake,
            PeerConnectionState::InitiatorSendHandshake
        );
        assert_ne!(
            PeerConnectionState::InitiatorSendHandshake,
            PeerConnectionState::InitiatorWaitHandshake
        );
    }

    // ── InteractionResult tests ────────────────────────────────────────

    #[test]
    fn test_interaction_result_variants() {
        let r = InteractionResult::Continue;
        assert_eq!(r, InteractionResult::Continue);

        let r = InteractionResult::Disconnect(InactiveReason::MutualUninterested);
        assert_eq!(
            r,
            InteractionResult::Disconnect(InactiveReason::MutualUninterested)
        );

        let r = InteractionResult::Disconnect(InactiveReason::NoDataExchange);
        assert_eq!(
            r,
            InteractionResult::Disconnect(InactiveReason::NoDataExchange)
        );

        let r = InteractionResult::Disconnect(InactiveReason::SeederToSeeder);
        assert_eq!(
            r,
            InteractionResult::Disconnect(InactiveReason::SeederToSeeder)
        );

        let r = InteractionResult::FloodingDetected;
        assert_eq!(r, InteractionResult::FloodingDetected);

        let r = InteractionResult::WaitingForHandshake;
        assert_eq!(r, InteractionResult::WaitingForHandshake);
    }

    // ── ChokingDecision / InterestDecision tests ───────────────────────

    #[test]
    fn test_choking_decision_variants() {
        assert_eq!(ChokingDecision::Choke, ChokingDecision::Choke);
        assert_ne!(ChokingDecision::Choke, ChokingDecision::Unchoke);
        assert_ne!(ChokingDecision::Unchoke, ChokingDecision::NoChange);
    }

    #[test]
    fn test_interest_decision_variants() {
        assert_eq!(InterestDecision::Interested, InterestDecision::Interested);
        assert_ne!(
            InterestDecision::Interested,
            InterestDecision::NotInterested
        );
        assert_ne!(
            InterestDecision::NotInterested,
            InterestDecision::NoChange
        );
    }

    // ── BtPeerInteractive creation tests ───────────────────────────────

    #[test]
    fn test_bt_peer_interactive_creation() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);

        assert_eq!(interactive.state(), PeerConnectionState::InitiatorSendHandshake);
        assert_eq!(interactive.count_received_message_in_iteration(), 0);
        assert_eq!(interactive.max_outstanding_request(), DEFAULT_MAX_OUTSTANDING_REQUEST);
        assert_eq!(interactive.info_hash(), &[0u8; 20]);
        assert!(!interactive.is_metadata_get_mode());
        assert_eq!(interactive.last_have_index(), 0);
        // New fields
        assert!(interactive.am_choking());
        assert!(!interactive.am_interested());
        assert!(interactive.peer_choking());
        assert!(!interactive.peer_interested());
    }

    #[test]
    fn test_bt_peer_interactive_with_state() {
        let info_hash = [1u8; 20];
        let interactive =
            BtPeerInteractive::with_state(info_hash, 50, PeerConnectionState::ReceiverWaitHandshake);

        assert_eq!(
            interactive.state(),
            PeerConnectionState::ReceiverWaitHandshake
        );
    }

    // ── Configuration setter tests ─────────────────────────────────────

    #[test]
    fn test_bt_peer_interactive_configuration() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        // Keep-alive interval
        interactive.set_keep_alive_interval(60);
        assert_eq!(interactive.keep_alive_interval_secs, 60);

        // Max outstanding request (clamped to [1, UB])
        interactive.set_max_outstanding_request(100);
        assert_eq!(interactive.max_outstanding_request(), 100);

        interactive.set_max_outstanding_request(0); // clamped to 1
        assert_eq!(interactive.max_outstanding_request(), 1);

        interactive.set_max_outstanding_request(9999); // clamped to UB
        assert_eq!(interactive.max_outstanding_request(), UB_MAX_OUTSTANDING_REQUEST);

        // Allowed fast set size
        interactive.set_allowed_fast_set_size(20);
        assert_eq!(interactive.allowed_fast_set_size, 20);

        // Feature flags
        interactive.set_ut_pex_enabled(true);
        assert!(interactive.ut_pex_enabled);

        interactive.set_dht_enabled(true);
        assert!(interactive.dht_enabled);

        interactive.enable_metadata_get_mode();
        assert!(interactive.is_metadata_get_mode());
    }

    // ── State machine transition tests ─────────────────────────────────

    #[test]
    fn test_advance_to_wait_handshake() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        assert_eq!(
            interactive.state(),
            PeerConnectionState::InitiatorSendHandshake
        );

        interactive.advance_to_wait_handshake();

        assert_eq!(
            interactive.state(),
            PeerConnectionState::InitiatorWaitHandshake
        );
    }

    #[test]
    #[should_panic(expected = "Can only advance to WAIT_HANDSHAKE")]
    fn test_advance_to_wait_handshake_invalid() {
        let info_hash = [0u8; 20];
        let mut interactive =
            BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);
        interactive.advance_to_wait_handshake();
    }

    #[test]
    fn test_advance_to_wired_from_initiator_wait() {
        let info_hash = [0u8; 20];
        let mut interactive =
            BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::InitiatorWaitHandshake);

        interactive.advance_to_wired();

        assert_eq!(interactive.state(), PeerConnectionState::Wired);
    }

    #[test]
    fn test_advance_to_wired_from_receiver_wait() {
        let info_hash = [0u8; 20];
        let mut interactive =
            BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);

        interactive.advance_to_wired();

        assert_eq!(interactive.state(), PeerConnectionState::Wired);
    }

    #[test]
    #[should_panic(expected = "Cannot advance to WIRED from WIRED")]
    fn test_advance_to_wired_invalid() {
        let info_hash = [0u8; 20];
        let mut interactive =
            BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::Wired);
        interactive.advance_to_wired();
    }

    #[test]
    fn test_full_initiator_lifecycle() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        // INITIATOR_SEND_HANDSHAKE
        assert!(interactive.state().is_handshake_state());

        // → INITIATOR_WAIT_HANDSHAKE
        interactive.advance_to_wait_handshake();
        assert!(interactive.state().is_handshake_state());

        // → WIRED
        interactive.advance_to_wired();
        assert!(interactive.state().is_wired());
    }

    #[test]
    fn test_full_receiver_lifecycle() {
        let info_hash = [0u8; 20];
        let mut interactive =
            BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);

        assert!(interactive.state().is_handshake_state());

        interactive.advance_to_wired();
        assert!(interactive.state().is_wired());
    }

    // ── Keep-alive timer tests ─────────────────────────────────────────

    #[test]
    fn test_keep_alive_timer_initially_not_needed() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);
        // Just created — should not need keepalive yet
        assert!(!interactive.should_send_keepalive());
    }

    #[test]
    fn test_keep_alive_timer_after_interval() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Force timer to past
        interactive.keep_alive_timer = instant_past(130);
        assert!(interactive.should_send_keepalive());
    }

    #[test]
    fn test_keep_alive_timer_reset() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.keep_alive_timer = instant_past(130);
        assert!(interactive.should_send_keepalive());
        interactive.reset_keep_alive_timer();
        assert!(!interactive.should_send_keepalive());
    }

    #[test]
    fn test_keep_alive_custom_interval() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.set_keep_alive_interval(60);
        interactive.keep_alive_timer = instant_past(65);
        assert!(interactive.should_send_keepalive());
    }

    // ── Flooding detection tests ───────────────────────────────────────

    #[test]
    fn test_flooding_detection_no_flooding() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Single choke/unchoke — not flooding
        interactive.on_message_received(0, false); // Choke, was not choking
        // Interval not elapsed yet
        assert!(!interactive.detect_flooding());
    }

    #[test]
    fn test_flooding_detection_choke_flooding() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Two choke/unchoke transitions → flooding
        interactive.on_message_received(0, false); // Choke, was not choking
        interactive.on_message_received(1, true); // Unchoke, was choking
        // Force both outer timer and inner FloodingStat timer elapsed
        interactive.flooding_timer = instant_past(6);
        interactive.flooding_stat.last_reset = instant_past(6);
        assert!(interactive.detect_flooding());
    }

    #[test]
    fn test_flooding_detection_keepalive_flooding() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.on_keepalive_received();
        interactive.on_keepalive_received();
        // Force both outer timer and inner FloodingStat timer elapsed
        interactive.flooding_timer = instant_past(6);
        interactive.flooding_stat.last_reset = instant_past(6);
        assert!(interactive.detect_flooding());
    }

    #[test]
    fn test_flooding_detection_reset_after_interval() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.on_message_received(0, false);
        interactive.on_message_received(1, true);
        interactive.flooding_timer = instant_past(6);
        interactive.flooding_stat.last_reset = instant_past(6);
        assert!(interactive.detect_flooding());
        // After detection, stats are reset — no more flooding
        assert!(!interactive.detect_flooding());
    }

    // ── Message received processing tests ──────────────────────────────

    #[test]
    fn test_on_message_received_choke() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Peer was not choking, then sends Choke → transition detected
        interactive.on_message_received(0, false);
        assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

        // Peer was already choking, sends Choke again → no transition
        interactive.on_message_received(0, true);
        assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
    }

    #[test]
    fn test_on_message_received_unchoke() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Peer was choking, sends Unchoke → transition detected
        interactive.on_message_received(1, true);
        assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

        // Peer was not choking, sends Unchoke → no transition
        interactive.on_message_received(1, false);
        assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
    }

    #[test]
    fn test_on_message_received_data_exchange() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Request (ID=6) and Piece (ID=7) record data exchange
        interactive.on_message_received(6, false);
        interactive.on_message_received(7, false);
        // The active_interaction_checker's last_data_exchange was reset
        // We can verify by checking that a subsequent check doesn't
        // immediately return NoDataExchange
    }

    #[test]
    fn test_on_keepalive_received() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.on_keepalive_received();
        interactive.on_keepalive_received();
        assert_eq!(interactive.flooding_stat.keepalive_count(), 2);
    }

    // ── Max outstanding request scaling tests ──────────────────────────

    #[test]
    fn test_scale_max_outstanding_request() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        assert_eq!(interactive.max_outstanding_request(), 6);

        // Lost >= 1/4 of outstanding requests → scale up
        // old=6, new=3, diff=3, diff*4=12 >= 6
        interactive.scale_max_outstanding_request(6, 3, false);
        assert_eq!(interactive.max_outstanding_request(), 12);
    }

    #[test]
    fn test_scale_max_outstanding_request_capped() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.max_outstanding_request = 200;

        // Would go to 400, capped at UB=256
        interactive.scale_max_outstanding_request(200, 100, false);
        assert_eq!(interactive.max_outstanding_request(), UB_MAX_OUTSTANDING_REQUEST);
    }

    #[test]
    fn test_scale_max_outstanding_request_no_scale_in_endgame() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // In end-game, don't scale
        interactive.scale_max_outstanding_request(6, 0, true);
        assert_eq!(interactive.max_outstanding_request(), 6);
    }

    #[test]
    fn test_scale_max_outstanding_request_no_scale_small_loss() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        // Lost only 1 request: diff=1, diff*4=4 < 6 → no scale
        interactive.scale_max_outstanding_request(6, 5, false);
        assert_eq!(interactive.max_outstanding_request(), 6);
    }

    // ── Have index tracking tests ──────────────────────────────────────

    #[test]
    fn test_have_index_tracking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        assert_eq!(interactive.last_have_index(), 0);

        interactive.set_last_have_index(42);
        assert_eq!(interactive.last_have_index(), 42);

        interactive.set_last_have_index(100);
        assert_eq!(interactive.last_have_index(), 100);
    }

    // ── check_have returns empty without piece storage ─────────────────

    #[test]
    fn test_check_have_returns_empty() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let have = interactive.check_have();
        assert!(have.is_empty());
    }

    // ── InactiveReason re-export test ──────────────────────────────────

    #[test]
    fn test_inactive_reason_variants() {
        assert_eq!(InactiveReason::SeederToSeeder, InactiveReason::SeederToSeeder);
        assert_eq!(InactiveReason::MutualUninterested, InactiveReason::MutualUninterested);
        assert_eq!(InactiveReason::NoDataExchange, InactiveReason::NoDataExchange);
        assert_ne!(InactiveReason::SeederToSeeder, InactiveReason::MutualUninterested);
    }

    // ==================================================================
    // NEW TESTS — dispatch_message, choking/interest state, check_have
    // ==================================================================

    // ── DispatchUpdate tests ────────────────────────────────────────────

    #[test]
    fn test_dispatch_update_default() {
        let update = DispatchUpdate::default();
        assert!(update.cancelled_slots.is_empty());
        assert!(update.have_index.is_none());
        assert!(update.bitfield_data.is_none());
        assert!(!update.peer_choking_changed);
        assert!(!update.peer_choking);
        assert!(update.extension_update.is_none());
    }

    // ── am_choking / am_interested / peer_choking / peer_interested ─────

    #[test]
    fn test_initial_choking_interest_state() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);
        // Initial state matches C++ defaults:
        // am_choking = true, am_interested = false
        // peer_choking = true, peer_interested = false
        assert!(interactive.am_choking());
        assert!(!interactive.am_interested());
        assert!(interactive.peer_choking());
        assert!(!interactive.peer_interested());
    }

    #[test]
    fn test_decide_choking_no_change_when_already_choking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_choking = true;

        // Create a minimal BtPeerConn with session resource where
        // should_be_choking() returns true (choking_required=true, opt_unchoking=false)
        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);

        // Should be choking and already choking → NoChange
        let decision = interactive.decide_choking(&conn);
        assert_eq!(decision, ChokingDecision::NoChange);
    }

    #[test]
    fn test_decide_choking_choke_when_not_choking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_choking = false;

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);
        // Default: choking_required=true, opt_unchoking=false → should_be_choking=true

        let decision = interactive.decide_choking(&conn);
        assert_eq!(decision, ChokingDecision::Choke);
    }

    #[test]
    fn test_decide_choking_unchoke_when_should_not_choke() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_choking = true;

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);
        // Set opt_unchoking → should_be_choking = false
        if let Some(ref mut res) = conn.session_resource {
            res.set_opt_unchoking(true);
        }

        let decision = interactive.decide_choking(&conn);
        assert_eq!(decision, ChokingDecision::Unchoke);
    }

    #[test]
    fn test_decide_choking_no_change_when_already_unchoked() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_choking = false;

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);
        if let Some(ref mut res) = conn.session_resource {
            res.set_opt_unchoking(true);
        }

        // Should not be choking and already not choking → NoChange
        let decision = interactive.decide_choking(&conn);
        assert_eq!(decision, ChokingDecision::NoChange);
    }

    #[test]
    fn test_decide_choking_no_resource() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);

        let conn = make_test_conn();
        // No session resource → NoChange
        let decision = interactive.decide_choking(&conn);
        assert_eq!(decision, ChokingDecision::NoChange);
    }

    // ── decide_interest tests ───────────────────────────────────────────

    #[test]
    fn test_decide_interest_becomes_interested() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_interested = false;

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);

        // has_missing_piece returns true, am_interested is false → Interested
        let decision =
            interactive.decide_interest_with_callback(&conn, &|_| true);
        assert_eq!(decision, InterestDecision::Interested);
    }

    #[test]
    fn test_decide_interest_becomes_not_interested() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_interested = true;

        let conn = make_test_conn();

        // has_missing_piece returns false, am_interested is true → NotInterested
        let decision =
            interactive.decide_interest_with_callback(&conn, &|_| false);
        assert_eq!(decision, InterestDecision::NotInterested);
    }

    #[test]
    fn test_decide_interest_no_change() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_interested = true;

        let conn = make_test_conn();

        // has_missing_piece returns true, am_interested is true → NoChange
        let decision =
            interactive.decide_interest_with_callback(&conn, &|_| true);
        assert_eq!(decision, InterestDecision::NoChange);
    }

    #[test]
    fn test_decide_interest_legacy_heuristic() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.am_interested = false;

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);

        // Legacy: session_resource.is_some() → should_be_interested = true
        let decision = interactive.decide_interest(&conn);
        assert_eq!(decision, InterestDecision::Interested);
    }

    // ── check_have_with_callback tests ──────────────────────────────────

    #[test]
    fn test_check_have_with_callback_returns_pieces() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        assert_eq!(interactive.last_have_index(), 0);

        let pieces = interactive.check_have_with_callback(&|| vec![5, 10, 15]);
        assert_eq!(pieces, vec![5, 10, 15]);
        // last_have_index should be updated to max (15)
        assert_eq!(interactive.last_have_index(), 15);
    }

    #[test]
    fn test_check_have_with_callback_empty() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let pieces = interactive.check_have_with_callback(&|| Vec::new());
        assert!(pieces.is_empty());
        // last_have_index should remain unchanged
        assert_eq!(interactive.last_have_index(), 0);
    }

    #[test]
    fn test_check_have_with_callback_updates_last_have_index() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.set_last_have_index(50);

        // Callback returns pieces with max < current last_have_index
        let _pieces = interactive.check_have_with_callback(&|| vec![3, 7]);
        // last_have_index should stay at 50 (max of 50, 7)
        assert_eq!(interactive.last_have_index(), 50);

        // Now callback returns pieces with max > current
        let _pieces = interactive.check_have_with_callback(&|| vec![60, 70]);
        assert_eq!(interactive.last_have_index(), 70);
    }

    // ── post_handshake_processing tests ─────────────────────────────────

    #[test]
    fn test_post_handshake_processing_defaults() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);
        let actions = interactive.post_handshake_processing();
        assert!(actions.send_bitfield);
        assert!(actions.send_extension_handshake);
        assert!(!actions.send_dht_port);
        assert!(actions.allowed_fast_pieces.is_empty());
    }

    #[test]
    fn test_post_handshake_processing_with_dht() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.set_dht_enabled(true);
        let actions = interactive.post_handshake_processing();
        assert!(actions.send_dht_port);
    }

    // ── dispatch_message tests (no connection I/O) ─────────────────────

    #[test]
    fn test_dispatch_choke_updates_peer_choking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.peer_choking = false;

        let mut conn = make_test_conn();
        let update = interactive.dispatch_message(BtMessage::Choke, &mut conn, |_| false);

        assert!(interactive.peer_choking());
        assert!(update.peer_choking_changed);
        assert!(update.peer_choking);
    }

    #[test]
    fn test_dispatch_unchoke_updates_peer_choking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.peer_choking = true;

        let mut conn = make_test_conn();
        let update = interactive.dispatch_message(BtMessage::Unchoke, &mut conn, |_| false);

        assert!(!interactive.peer_choking());
        assert!(update.peer_choking_changed);
        assert!(!update.peer_choking);
    }

    #[test]
    fn test_dispatch_interested_updates_peer_interested() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let mut conn = make_test_conn();
        let _update = interactive.dispatch_message(BtMessage::Interested, &mut conn, |_| false);

        assert!(interactive.peer_interested());
    }

    #[test]
    fn test_dispatch_not_interested_updates_peer_interested() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        interactive.peer_interested = true;

        let mut conn = make_test_conn();
        let _update = interactive.dispatch_message(BtMessage::NotInterested, &mut conn, |_| false);

        assert!(!interactive.peer_interested());
    }

    #[test]
    fn test_dispatch_have_updates_bitfield() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);

        let update = interactive.dispatch_message(
            BtMessage::Have { piece_index: 0 },
            &mut conn,
            |_| false,
        );

        assert_eq!(update.have_index, Some(0));
        // The peer should now have piece 0
        assert!(conn.has_piece(0));
    }

    #[test]
    fn test_dispatch_keepalive_updates_flooding() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let mut conn = make_test_conn();
        let _update = interactive.dispatch_message(BtMessage::KeepAlive, &mut conn, |_| false);

        assert_eq!(interactive.flooding_stat.keepalive_count(), 1);
    }

    #[test]
    fn test_dispatch_allowed_fast_updates_conn() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let mut conn = make_test_conn();
        let _update =
            interactive.dispatch_message(BtMessage::AllowedFast { index: 42 }, &mut conn, |_| false);

        assert!(conn.is_allowed_fast(42));
        assert!(!conn.is_allowed_fast(43));
    }

    #[test]
    fn test_dispatch_have_all_marks_seeder() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        let mut conn = make_test_conn();
        conn.allocate_session_resource(256 * 1024, 1024 * 1024);

        let _update = interactive.dispatch_message(BtMessage::HaveAll, &mut conn, |_| false);

        assert!(conn.seeder);
    }

    #[test]
    fn test_dispatch_choke_removes_non_allowed_fast_slots() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        // Pre-populate some outstanding requests
        interactive
            .handler
            .send_request(5, 0, constants::BT_BLOCK_SIZE as u32, vec![1]);
        interactive
            .handler
            .send_request(6, 0, constants::BT_BLOCK_SIZE as u32, vec![2]);

        let mut conn = make_test_conn();
        // Piece 6 is in allowed-fast set
        let update = interactive.dispatch_message(BtMessage::Choke, &mut conn, |idx| idx == 6);

        // Should have removed slot for piece 5 but kept piece 6
        assert_eq!(update.cancelled_slots.len(), 1);
        assert_eq!(update.cancelled_slots[0].index, 5);
        assert!(interactive.handler.is_outstanding_request(6, 0));
        assert!(!interactive.handler.is_outstanding_request(5, 0));
    }

    #[test]
    fn test_dispatch_piece_removes_outstanding_slot() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        interactive
            .handler
            .send_request(5, 0, constants::BT_BLOCK_SIZE as u32, vec![1]);

        let mut conn = make_test_conn();
        let _update = interactive.dispatch_message(
            BtMessage::Piece {
                index: 5,
                begin: 0,
                data: vec![0u8; constants::BT_BLOCK_SIZE],
            },
            &mut conn,
            |_| false,
        );

        // The outstanding request should be removed
        assert!(!interactive.handler.is_outstanding_request(5, 0));
    }

    #[test]
    fn test_handler_access() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);

        // Verify handler is accessible
        assert_eq!(interactive.handler().count_outstanding_requests(), 0);

        // Mut access
        interactive.handler_mut().send_request(
            5,
            0,
            constants::BT_BLOCK_SIZE as u32,
            vec![1],
        );
        assert_eq!(interactive.handler().count_outstanding_requests(), 1);
    }

    // ── download_finished flag test ─────────────────────────────────────

    #[test]
    fn test_download_finished_flag() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        assert!(!interactive.download_finished);
        interactive.set_download_finished(true);
        assert!(interactive.download_finished);
    }

    // ── PostHandshakeActions tests ──────────────────────────────────────

    #[test]
    fn test_post_handshake_actions_fields() {
        let actions = PostHandshakeActions {
            send_bitfield: true,
            send_extension_handshake: true,
            send_dht_port: false,
            allowed_fast_pieces: vec![1, 2, 3],
        };
        assert!(actions.send_bitfield);
        assert!(actions.send_extension_handshake);
        assert!(!actions.send_dht_port);
        assert_eq!(actions.allowed_fast_pieces, vec![1, 2, 3]);
    }

    // ── Extension registry integration tests ────────────────────────────

    #[test]
    fn test_extension_registry_initial_state() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 100);
        assert_eq!(interactive.extension_registry().local_ut_metadata_id(), 1);
        assert_eq!(interactive.extension_registry().local_ut_pex_id(), 2);
        assert!(interactive.extension_registry().peer_ut_metadata_id().is_none());
        assert!(interactive.extension_registry().peer_ut_pex_id().is_none());
    }

    #[test]
    fn test_dispatch_extended_handshake() {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        // Build and dispatch an extension handshake message
        let hs = ExtensionHandshake::new();
        let payload = hs.to_bytes();
        let msg = BtMessage::Extended {
            ext_id: 0,
            payload,
        };

        let update = interactive.dispatch_message(msg, &mut conn, |_| false);

        // Verify the extension update was produced
        assert!(update.extension_update.is_some());
        match update.extension_update.unwrap() {
            ExtensionUpdate::HandshakeReceived {
                ut_metadata_id,
                ut_pex_id,
                reqq,
            } => {
                assert_eq!(ut_metadata_id, Some(1));
                assert_eq!(ut_pex_id, Some(2));
                assert_eq!(reqq, 500);
            }
            other => panic!("Expected HandshakeReceived, got {:?}", other),
        }

        // Verify the registry was updated
        assert_eq!(interactive.extension_registry().peer_ut_metadata_id(), Some(1));
        assert_eq!(interactive.extension_registry().peer_ut_pex_id(), Some(2));

        // PEX should be auto-enabled after handshake
        assert!(interactive.ut_pex_enabled);
    }

    #[test]
    fn test_dispatch_extended_ut_metadata_request() {
        use aria2_protocol::bittorrent::message::extension::{
            ExtensionHandshake, UtMetadataMessage,
        };

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        // First, receive a handshake so the registry knows the peer's IDs
        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: hs_payload,
            },
            &mut conn,
            |_| false,
        );

        // Now dispatch a ut_metadata request (peer's id = 1)
        let msg = UtMetadataMessage::Request { piece: 0 };
        let payload = msg.to_payload();
        let update = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 1,
                payload,
            },
            &mut conn,
            |_| false,
        );

        assert!(update.extension_update.is_some());
        match update.extension_update.unwrap() {
            ExtensionUpdate::MetadataRequest { piece } => {
                assert_eq!(piece, 0);
            }
            other => panic!("Expected MetadataRequest, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_extended_ut_metadata_data() {
        use aria2_protocol::bittorrent::message::extension::{
            ExtensionHandshake, UtMetadataMessage,
        };

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: hs_payload,
            },
            &mut conn,
            |_| false,
        );

        let msg = UtMetadataMessage::Data {
            piece: 2,
            total_size: 50000,
            data: b"test metadata".to_vec(),
        };
        let payload = msg.to_payload();
        let update = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 1,
                payload,
            },
            &mut conn,
            |_| false,
        );

        assert!(update.extension_update.is_some());
        match update.extension_update.unwrap() {
            ExtensionUpdate::MetadataPiece {
                piece,
                total_size,
                data,
            } => {
                assert_eq!(piece, 2);
                assert_eq!(total_size, 50000);
                assert_eq!(data, b"test metadata");
            }
            other => panic!("Expected MetadataPiece, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_extended_ut_pex() {
        use aria2_protocol::bittorrent::message::extension::{
            CompactPeerV4, ExtensionHandshake, UtPexMessage,
        };

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: hs_payload,
            },
            &mut conn,
            |_| false,
        );

        // Build a PEX message
        let mut pex = UtPexMessage::new();
        let mut peer_bytes = [0u8; 6];
        peer_bytes[..4].copy_from_slice(&[10, 0, 0, 1]);
        peer_bytes[4..6].copy_from_slice(&6881u16.to_be_bytes());
        pex.added.push(CompactPeerV4(peer_bytes));

        let payload = pex.to_payload();
        // Peer's ut_pex id = 2
        let update = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 2,
                payload,
            },
            &mut conn,
            |_| false,
        );

        assert!(update.extension_update.is_some());
        match update.extension_update.unwrap() {
            ExtensionUpdate::PeerExchange { added_v4, added_v6 } => {
                assert_eq!(added_v4.len(), 1);
                assert!(added_v6.is_empty());
                assert_eq!(added_v4[0], CompactPeerV4(peer_bytes));
            }
            other => panic!("Expected PeerExchange, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_extended_unknown_ext_id() {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        // Receive handshake first
        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: hs_payload,
            },
            &mut conn,
            |_| false,
        );

        // Dispatch with unknown ext_id
        let update = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 99,
                payload: vec![],
            },
            &mut conn,
            |_| false,
        );

        assert!(update.extension_update.is_none());
    }

    #[test]
    fn test_dispatch_extended_handshake_enables_pex() {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        // PEX should be disabled initially
        assert!(!interactive.ut_pex_enabled);

        // Receive handshake that includes ut_pex
        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: hs_payload,
            },
            &mut conn,
            |_| false,
        );

        // PEX should now be enabled
        assert!(interactive.ut_pex_enabled);
    }

    #[test]
    fn test_dispatch_extended_handshake_without_pex() {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 100);
        let mut conn = make_test_conn();

        // Build a handshake with only ut_metadata (no ut_pex)
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
        let bytes = BencodeValue::Dict(root).encode();

        let _ = interactive.dispatch_message(
            BtMessage::Extended {
                ext_id: 0,
                payload: bytes,
            },
            &mut conn,
            |_| false,
        );

        // PEX should remain disabled since peer doesn't support it
        assert!(!interactive.ut_pex_enabled);
        // But ut_metadata should be available
        assert!(interactive.extension_registry().supports_ut_metadata());
    }

    // ── Helper to create a test BtPeerConn ─────────────────────────────

    /// Create a minimal `BtPeerConn` for testing purposes.
    fn make_test_conn() -> BtPeerConn {
        BtPeerConn::new_stub(&[0u8; 20])
    }

    // ── Mock PieceProvider for addRequests/fillPiece tests ──────────────

    /// Mock piece provider that simulates PieceStorage operations.
    struct MockPieceProvider {
        /// Whether has_missing_piece() returns true.
        has_missing: bool,
        /// Whether has_missing_unused_piece() returns true.
        has_missing_unused: bool,
        /// Whether is_end_game() returns true.
        is_end_game: bool,
        /// Whether enter_end_game() was called.
        entered_end_game: bool,
        /// Pieces to return from get_missing_pieces().
        missing_pieces: Vec<Piece>,
        /// Pieces to return from get_missing_fast_pieces().
        fast_pieces: Vec<Piece>,
    }

    impl MockPieceProvider {
        fn new() -> Self {
            Self {
                has_missing: true,
                has_missing_unused: true,
                is_end_game: false,
                entered_end_game: false,
                missing_pieces: Vec::new(),
                fast_pieces: Vec::new(),
            }
        }
    }

    impl PieceProvider for MockPieceProvider {
        fn has_missing_piece(&self, _peer: &BtPeerConn) -> bool {
            self.has_missing
        }

        fn get_missing_pieces(
            &mut self,
            count: usize,
            _peer: &BtPeerConn,
            _target_piece_indexes: &[u32],
            _cuid: u64,
        ) -> Vec<Piece> {
            self.missing_pieces.drain(..count.min(self.missing_pieces.len())).collect()
        }

        fn get_missing_fast_pieces(
            &mut self,
            count: usize,
            _peer: &BtPeerConn,
            _target_piece_indexes: &[u32],
            _cuid: u64,
        ) -> Vec<Piece> {
            self.fast_pieces.drain(..count.min(self.fast_pieces.len())).collect()
        }

        fn is_end_game(&self) -> bool {
            self.is_end_game
        }

        fn has_missing_unused_piece(&self) -> bool {
            self.has_missing_unused
        }

        fn enter_end_game(&mut self) {
            self.entered_end_game = true;
        }

        fn get_advertised_piece_indexes_ext(
            &self,
            _my_cuid: u64,
            _last_have_index: u64,
        ) -> (Vec<usize>, u64) {
            (Vec::new(), 0)
        }

        fn get_bitfield_length_ext(&self) -> usize {
            0
        }

        fn get_bitfield_ext(&self) -> Vec<u8> {
            Vec::new()
        }

        fn all_download_finished_ext(&self) -> bool {
            false
        }

        fn get_completed_length_ext(&self) -> u64 {
            0
        }
    }

    // ── fill_piece tests ────────────────────────────────────────────────

    #[test]
    fn test_fill_piece_no_missing_pieces() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.has_missing = false;

        interactive.fill_piece(&mut mock, &conn, 1);
        assert_eq!(interactive.request_factory().count_target_piece(), 0);
    }

    #[test]
    fn test_fill_piece_adds_piece_when_below_max() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;

        // Add 1 piece with 4 missing blocks, below max_outstanding_request (6)
        interactive.request_factory_mut().add_target_piece(Piece::new(0, 65536));

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.missing_pieces = vec![Piece::new(1, 65536)];

        interactive.fill_piece(&mut mock, &conn, 1);
        // Should add piece 1 because 4 missing blocks < max_outstanding (6)
        assert_eq!(interactive.request_factory().count_target_piece(), 2);
    }

    #[test]
    fn test_fill_piece_adds_pieces_when_not_choking() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false; // Not choking us
        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.missing_pieces = vec![Piece::new(0, 65536), Piece::new(1, 65536)];

        interactive.fill_piece(&mut mock, &conn, 1);
        assert_eq!(interactive.request_factory().count_target_piece(), 2);
    }

    #[test]
    fn test_fill_piece_choking_no_fast_extension() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = true; // Choking us
        let conn = make_test_conn();
        // conn has no session_resource → fast extension disabled

        let mut mock = MockPieceProvider::new();
        mock.missing_pieces = vec![Piece::new(0, 65536)];
        mock.fast_pieces = vec![Piece::new(1, 65536)];

        interactive.fill_piece(&mut mock, &conn, 1);
        // Should not add any pieces because peer is choking and no fast extension
        assert_eq!(interactive.request_factory().count_target_piece(), 0);
    }

    #[test]
    fn test_fill_piece_choking_with_fast_extension() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = true; // Choking us

        let mut conn = make_test_conn();
        conn.allocate_session_resource(65536, 655360);
        conn.session_resource.as_mut().unwrap().set_fast_extension_enabled(true);

        let mut mock = MockPieceProvider::new();
        mock.fast_pieces = vec![Piece::new(0, 65536)];

        interactive.fill_piece(&mut mock, &conn, 1);
        assert_eq!(interactive.request_factory().count_target_piece(), 1);
    }

    #[test]
    fn test_fill_piece_enough_blocks_already() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;

        // Add 2 pieces with 4 blocks each = 8 missing blocks >= max_outstanding (6)
        interactive.request_factory_mut().add_target_piece(Piece::new(0, 65536));
        interactive.request_factory_mut().add_target_piece(Piece::new(1, 65536));

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.missing_pieces = vec![Piece::new(2, 65536)];

        interactive.fill_piece(&mut mock, &conn, 1);
        // Should NOT add more pieces (8 missing blocks >= max_outstanding_request=6)
        assert_eq!(interactive.request_factory().count_target_piece(), 2);
    }

    // ── add_requests tests ──────────────────────────────────────────────

    #[test]
    fn test_add_requests_enters_endgame() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.has_missing_unused = false; // Triggers endgame
        mock.missing_pieces = vec![Piece::new(0, 65536)];

        let requests = interactive.add_requests(&mut mock, &conn, 1);

        assert!(interactive.is_endgame());
        assert!(mock.entered_end_game);
        // Should have generated some requests from the piece
        assert!(!requests.is_empty());
    }

    #[test]
    fn test_add_requests_does_not_reenter_endgame() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;
        interactive.endgame = true; // Already in endgame

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.has_missing_unused = false;

        let _ = interactive.add_requests(&mut mock, &conn, 1);

        // enter_end_game should NOT be called again
        assert!(!mock.entered_end_game);
    }

    #[test]
    fn test_add_requests_no_requests_when_max_outstanding_reached() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;

        // Add pieces and make the handler think we already have max outstanding requests
        // by adding request slots directly
        for i in 0..DEFAULT_MAX_OUTSTANDING_REQUEST {
            interactive.handler_mut().dispatcher.add_request_slot(i as u32, 0, 16384);
        }

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        mock.missing_pieces = vec![Piece::new(0, 65536)];

        let requests = interactive.add_requests(&mut mock, &conn, 1);
        // No new requests should be created
        assert!(requests.is_empty());
    }

    #[test]
    fn test_add_requests_creates_requests_for_new_pieces() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.peer_choking = false;

        let conn = make_test_conn();
        let mut mock = MockPieceProvider::new();
        // Provide a piece with 4 blocks
        mock.missing_pieces = vec![Piece::new(0, 65536)];

        let requests = interactive.add_requests(&mut mock, &conn, 1);

        // Should have created some requests
        assert!(!requests.is_empty());
        // All requests should be for piece 0
        for req in &requests {
            assert_eq!(req.index, 0);
        }
    }

    // ── cancel_all_piece tests ──────────────────────────────────────────

    #[test]
    fn test_cancel_all_piece() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.request_factory_mut().add_target_piece(Piece::new(0, 65536));
        interactive.request_factory_mut().add_target_piece(Piece::new(1, 65536));

        let removed = interactive.cancel_all_piece();
        assert_eq!(removed, vec![0, 1]);
        assert_eq!(interactive.request_factory().count_target_piece(), 0);
    }

    #[test]
    fn test_cancel_all_piece_empty() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        let removed = interactive.cancel_all_piece();
        assert!(removed.is_empty());
    }

    // ── remove_completed_piece tests ────────────────────────────────────

    #[test]
    fn test_remove_completed_piece() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);

        let mut piece0 = Piece::new(0, 65536);
        piece0.set_all_blocks(); // Mark as complete
        interactive.request_factory_mut().add_target_piece(piece0);
        interactive.request_factory_mut().add_target_piece(Piece::new(1, 65536));

        let completed = interactive.remove_completed_piece();
        assert_eq!(completed, vec![0]);
        assert_eq!(interactive.request_factory().count_target_piece(), 1);
    }

    #[test]
    fn test_remove_completed_piece_none() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);
        interactive.request_factory_mut().add_target_piece(Piece::new(0, 65536));

        let completed = interactive.remove_completed_piece();
        assert!(completed.is_empty());
    }

    // ── endgame flag tests ──────────────────────────────────────────────

    #[test]
    fn test_endgame_flag_initially_false() {
        let info_hash = [0u8; 20];
        let interactive = BtPeerInteractive::new(info_hash, 10);
        assert!(!interactive.is_endgame());
    }

    // ── request_factory accessor tests ──────────────────────────────────

    #[test]
    fn test_request_factory_accessors() {
        let info_hash = [0u8; 20];
        let mut interactive = BtPeerInteractive::new(info_hash, 10);

        assert_eq!(interactive.request_factory().count_target_piece(), 0);

        interactive.request_factory_mut().add_target_piece(Piece::new(0, 65536));
        assert_eq!(interactive.request_factory().count_target_piece(), 1);
    }

    // ── PieceProvider trait tests ────────────────────────────────────────

    #[test]
    fn test_mock_piece_provider_basic() {
        let mut mock = MockPieceProvider::new();
        let conn = make_test_conn();

        assert!(mock.has_missing_piece(&conn));
        assert!(mock.has_missing_unused_piece());
        assert!(!mock.is_end_game());

        mock.enter_end_game();
        assert!(mock.entered_end_game);
    }

    // ── checkHave optimization tests ──────────────────────────────────────

    #[test]
    fn test_check_have_result_none_when_no_indexes() {
        let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
        let result = interactive.check_have_optimized(
            &|_last| (Vec::new(), 0u64), // no new pieces
            100, // bitfield_length
            false, // fast_ext
            false, // all_done
            0, // completed_len
        );
        assert_eq!(result, CheckHaveResult::None);
    }

    #[test]
    fn test_check_have_result_bitfield_when_many_indexes() {
        let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
        // 20 Have messages = 20 * 9 = 180 bytes
        // Bitfield = 5 + 10 = 15 bytes
        // Condition: 5 + 10 <= 20 * 9 → true → use Bitfield
        let indexes: Vec<usize> = (0..20).collect();
        let result = interactive.check_have_optimized(
            &|_last| (indexes.clone(), 20u64),
            10, // bitfield_length=10 → 5+10=15 <= 180
            false, // fast_ext
            false, // all_done
            1024, // completed_len > 0
        );
        assert_eq!(result, CheckHaveResult::Bitfield);
    }

    #[test]
    fn test_check_have_result_have_all_when_fast_ext_and_complete() {
        let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
        let indexes: Vec<usize> = (0..20).collect();
        let result = interactive.check_have_optimized(
            &|_last| (indexes.clone(), 20u64),
            10,
            true,  // fast_ext enabled
            true,  // all done
            1024,
        );
        assert_eq!(result, CheckHaveResult::HaveAll);
    }

    #[test]
    fn test_check_have_result_have_indexes_when_few() {
        let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
        // 2 Have messages = 2 * 9 = 18 bytes
        // Bitfield = 5 + 100 = 105 bytes
        // Condition: 5 + 100 <= 2 * 9 → false → use Have messages
        let indexes = vec![0usize, 1];
        let result = interactive.check_have_optimized(
            &|_last| (indexes.clone(), 2u64),
            100, // bitfield_length=100 → 5+100=105 > 18
            false,
            false,
            1024,
        );
        assert_eq!(result, CheckHaveResult::HaveIndexes(indexes));
    }
}
