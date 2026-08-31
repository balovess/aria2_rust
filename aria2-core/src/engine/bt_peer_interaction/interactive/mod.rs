//! BtPeerInteractive — per-peer interaction loop (C++ DefaultBtInteractive)
//!
//! This module contains the `BtPeerInteractive` struct and its implementation,
//! which manages the per-peer interaction processing loop in the BitTorrent
//! protocol.
//!
//! The implementation is split across sub-modules by thematic grouping:
//! - [`core`] — Constructor, configuration setters, state accessors, and
//!   state machine transitions.
//! - [`dispatch`] — Message dispatch, receive, and flooding detection/scaling
//!   helpers.
//! - [`decisions`] — Choking/interest decisions, check-have, keep-alive,
//!   peer exchange, and request generation.

pub mod core;
pub mod decisions;
pub mod dispatch;

use std::time::Instant;

use crate::engine::bt_message_dispatcher::{ActiveInteractionChecker, FloodingStat};
use crate::engine::bt_message_handler::BtPeerMessageHandler;
use crate::engine::bt_message_validation::BtMessageValidator;
use crate::engine::bt_request_factory::BtRequestFactory;
use crate::engine::extension_registry::ExtensionRegistry;

use super::types::*;

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
    pub(crate) state: PeerConnectionState,

    // ── Message handler (C++ DefaultBtMessageDispatcher per peer) ──────
    /// Per-peer message handler with request slot tracking and flooding.
    pub(crate) handler: BtPeerMessageHandler,

    // ── Peer state tracking (C++ Peer fields) ─────────────────────────
    /// Whether we are currently choking this peer (C++ `amChoking_`).
    pub(crate) am_choking: bool,
    /// Whether we are currently interested in this peer (C++ `amInterested_`).
    pub(crate) am_interested: bool,
    /// Whether the peer is currently choking us (C++ `peerChoking_`).
    pub(crate) peer_choking: bool,
    /// Whether the peer is currently interested in us (C++ `peerInterested_`).
    pub(crate) peer_interested: bool,

    // ── Timers (matching C++) ──────────────────────────────────────────
    /// Timer for keep-alive sending (C++ `keepAliveTimer_`).
    pub(crate) keep_alive_timer: Instant,
    /// Timer for flooding check interval (C++ `floodingTimer_`).
    pub(crate) flooding_timer: Instant,
    /// Timer for inactive peer detection (C++ `inactiveTimer_`).
    pub(crate) inactive_timer: Instant,
    /// Per-second timer for request slot checking (C++ `perSecTimer_`).
    pub(crate) per_sec_timer: Instant,
    /// Timer for peer exchange messages (C++ `pexTimer_`).
    pub(crate) pex_timer: Instant,

    // ── Configuration ──────────────────────────────────────────────────
    /// Keep-alive interval in seconds (C++ `keepAliveInterval_`, default 120).
    pub(crate) keep_alive_interval_secs: u64,
    /// Maximum outstanding piece requests (C++ `maxOutstandingRequest_`, default 6).
    pub(crate) max_outstanding_request: usize,
    /// Allowed-fast set size (C++ `allowedFastSetSize_`, default 10).
    pub(crate) allowed_fast_set_size: usize,

    // ── Flooding detection ─────────────────────────────────────────────
    /// Flooding statistics tracker.
    pub(crate) flooding_stat: FloodingStat,

    // ── Active interaction checking ────────────────────────────────────
    /// Inactive peer checker.
    pub(crate) active_interaction_checker: ActiveInteractionChecker,

    // ── Tracking ───────────────────────────────────────────────────────
    /// Last have index we have advertised to the peer (C++ `lastHaveIndex_`).
    pub(crate) last_have_index: u64,
    /// Number of messages received in the current iteration (C++ `numReceivedMessage_`).
    pub(crate) num_received_message: usize,
    /// Total number of pieces in the torrent.
    #[allow(dead_code)]
    pub(crate) num_pieces: u32,
    /// 20-byte info hash for this torrent.
    pub(crate) info_hash: [u8; 20],

    // ── Feature flags ──────────────────────────────────────────────────
    /// Whether UT PEX (peer exchange) is enabled (C++ `utPexEnabled_`).
    pub(crate) ut_pex_enabled: bool,
    /// Whether DHT is enabled (C++ `dhtEnabled_`).
    pub(crate) dht_enabled: bool,
    /// Callback for BEP 5 Port messages, equivalent to DHT context injection.
    pub(crate) dht_port_handler: Option<std::sync::Arc<dyn Fn(u16) + Send + Sync>>,
    /// Whether we are in metadata-get mode (C++ `metadataGetMode_`).
    pub(crate) metadata_get_mode: bool,
    /// Domain validator equivalent to DefaultBtMessageFactory's per-message validators.
    pub(crate) message_validator: Option<BtMessageValidator>,
    /// Whether the download is finished (affects addRequests step).
    pub(crate) download_finished: bool,

    // ── Extension Protocol (BEP 10) ──────────────────────────────────────
    /// Per-peer extension registry tracking local and peer ext_id assignments.
    pub(crate) extension_registry: ExtensionRegistry,
    /// Optional sink for factory-created extension message side effects.
    #[allow(clippy::type_complexity)]
    pub(crate) extension_update_handler: Option<
        std::sync::Arc<dyn Fn(&crate::engine::extension_registry::ExtensionUpdate) + Send + Sync>,
    >,

    // ── Request generation (C++ DefaultBtRequestFactory) ──────────────────
    /// Per-peer request factory managing target pieces and generating Request messages.
    /// Mirrors C++ `btRequestFactory_` in `DefaultBtInteractive`.
    pub(crate) request_factory: BtRequestFactory,

    /// Whether end-game mode has been entered for this download.
    /// Mirrors C++ `endGame_` in `DefaultBtInteractive`.
    pub(crate) endgame: bool,
}

// Re-export the struct so that `use crate::engine::bt_peer_interaction::interactive::BtPeerInteractive`
// still works (the struct is defined in this very file, so the re-export is implicit).
// All impl blocks are in the sub-modules; Rust allows multiple impl blocks
// across files within the same crate as long as they see the type definition.
