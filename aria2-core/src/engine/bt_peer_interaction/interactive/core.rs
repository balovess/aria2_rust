//! Core constructor, configuration setters, state accessors, and state machine
//! transitions for `BtPeerInteractive`.

use std::time::{Duration, Instant};

use crate::constants;
use crate::engine::bt_message_dispatcher::{ActiveInteractionChecker, FloodingStat};
use crate::engine::bt_message_handler::BtPeerMessageHandler;
use crate::engine::bt_request_factory::BtRequestFactory;
use crate::engine::extension_registry::ExtensionRegistry;
use aria2_protocol::bittorrent::message::validation::BtMessageValidator;
use tracing::debug;

use super::super::types::*;
use super::BtPeerInteractive;

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
            dht_port_handler: None,
            metadata_get_mode: false,
            message_validator: None,
            download_finished: false,
            extension_registry: ExtensionRegistry::new(),
            extension_update_handler: None,
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
        self.max_outstanding_request = max.clamp(1, UB_MAX_OUTSTANDING_REQUEST);
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

    pub fn set_dht_port_handler<F>(&mut self, handler: F)
    where
        F: Fn(u16) + Send + Sync + 'static,
    {
        self.dht_port_handler = Some(std::sync::Arc::new(handler));
    }

    pub fn set_piece_length(&mut self, piece_length: u32) {
        if let Some(validator) = &mut self.message_validator {
            validator.piece_length = piece_length;
        }
    }

    pub fn configure_message_validator(&mut self, piece_length: u32) {
        let mut validator = BtMessageValidator::new(self.num_pieces, piece_length);
        validator.metadata_get_mode = self.metadata_get_mode;
        self.message_validator = Some(validator);
    }

    pub fn set_metadata_get_mode(&mut self, enabled: bool) {
        self.metadata_get_mode = enabled;
        if let Some(validator) = &mut self.message_validator {
            validator.metadata_get_mode = enabled;
        }
    }

    /// Enable metadata-get mode.
    /// Matches C++ `enableMetadataGetMode()`.
    pub fn enable_metadata_get_mode(&mut self) {
        self.set_metadata_get_mode(true);
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

    pub fn set_extension_update_handler<F>(&mut self, handler: F)
    where
        F: Fn(&crate::engine::extension_registry::ExtensionUpdate) + Send + Sync + 'static,
    {
        self.extension_update_handler = Some(std::sync::Arc::new(handler));
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
    /// The actual message sending is done by the caller using the connection.
    /// This method returns a summary of what should be sent so the caller
    /// can decide.
    ///
    /// # Arguments
    ///
    /// * `peer_ip` — The peer's IP address, used to compute the BEP 6
    ///   allowed-fast set. Pass `None` if the IP is unavailable (fast set
    ///   will be empty).
    ///
    /// # Returns
    ///
    /// A [`PostHandshakeActions`] describing what messages should be sent.
    pub fn post_handshake_processing(&self, peer_ip: Option<&str>) -> PostHandshakeActions {
        let allowed_fast_pieces = match peer_ip {
            Some(ip) => aria2_protocol::bittorrent::fast_set::compute_fast_set(
                ip,
                self.num_pieces,
                &self.info_hash,
                self.allowed_fast_set_size,
            ),
            None => Vec::new(),
        };
        PostHandshakeActions {
            send_bitfield: true,
            // Send extension handshake if we have local extensions configured
            send_extension_handshake: true,
            send_dht_port: self.dht_enabled,
            allowed_fast_pieces,
        }
    }
}
