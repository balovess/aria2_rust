//! Type definitions for the BT peer interaction module.
//!
//! Contains enums, structs, and constants used by the peer interaction
//! loop and connection lifecycle.

use crate::engine::bt_message_dispatcher::{InactiveReason, RequestSlot};
use crate::engine::extension_registry::ExtensionUpdate;

// ======================================================================
// Constants (matching C++ aria2)
// ======================================================================

/// Delay between peer connection setup and message reading (milliseconds)
pub const PEER_CONNECTION_DELAY_MS: u64 = crate::constants::BT_PEER_CONNECTION_DELAY_MS;

/// Maximum attempts to wait for unchoke from a peer
pub const MAX_UNCHOKE_WAIT_ATTEMPTS: u32 = crate::constants::BT_MAX_UNCHOKE_WAIT_ATTEMPTS as u32;

/// Timeout for each message read from peer (seconds)
pub const PEER_MESSAGE_TIMEOUT_SECS: u64 = crate::constants::BT_PEER_MESSAGE_TIMEOUT_SECS;

/// Default maximum number of outstanding piece requests per peer.
/// Matches C++ `DEFAULT_MAX_OUTSTANDING_REQUEST = 6` in BtConstants.h.
pub const DEFAULT_MAX_OUTSTANDING_REQUEST: usize =
    crate::constants::BT_DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Upper bound for max outstanding requests (dynamic scaling ceiling).
/// Matches C++ `UB_MAX_OUTSTANDING_REQUEST = 256` in BtConstants.h.
pub const UB_MAX_OUTSTANDING_REQUEST: usize = crate::constants::BT_UB_MAX_OUTSTANDING_REQUEST;

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
#[derive(Debug, Clone)]
pub enum InteractionResult {
    /// Normal processing completed; continue next iteration.
    /// `pex_pending` is true when the PEX timer fired and the caller should
    /// build and send a PEX message (BEP 11) to this peer.
    /// `pex_update` carries an inbound `ExtensionUpdate::PeerExchange` if one
    /// was received during this tick; the caller should add the discovered
    /// peers to its known-peers list.
    Continue {
        /// Whether a PEX (Peer Exchange) message is due for this peer.
        /// When true, the caller should build a PEX message from the known
        /// peers list and queue it for sending. This matches C++
        /// `DefaultBtInteractive::addPeerExchangeMessage()`.
        pex_pending: bool,
        /// Inbound PEX update (BEP 11) received during this tick, if any.
        /// The caller should extract discovered peers and add them to the
        /// known-peers list for potential connection.
        pex_update: Option<crate::engine::extension_registry::ExtensionUpdate>,
    },
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
#[derive(Default)]
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


// ======================================================================
// PeerIdCheckResult — same-peer-ID duplicate detection
// ======================================================================

/// Result of checking a received peer ID for self-connection or duplicates.
///
/// Mirrors the two checks in C++ `DefaultBtInteractive::receiveHandshake()`:
/// 1. `memcmp(message->getPeerId(), bittorrent::getStaticPeerId(), PEER_ID_LENGTH) == 0`
///    — remote peer ID matches our own (self-connection).
/// 2. Iterating `peerStorage_->getUsedPeers()` for an active peer with the
///    same ID — duplicate connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerIdCheckResult {
    /// The remote peer ID matches our own static peer ID (self-connection).
    /// C++ throws: "Drop connection from the same Peer ID"
    SelfConnection,
    /// The remote peer ID is already connected on another active peer.
    /// C++ throws: "Same Peer ID has been already seen."
    DuplicatePeer,
    /// The peer ID is unique — proceed with the handshake.
    Ok,
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

use crate::engine::bt_peer_connection::BtPeerConn;

/// Result of peer connection attempt
pub struct PeerConnectionResult {
    /// Successfully connected peers
    pub connections: Vec<BtPeerConn>,
    /// Number of failed connections
    pub failed_count: usize,
}
