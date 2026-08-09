//! Extension protocol handlers — fast extension, metadata mode, DHT port.

use tracing::{debug, trace, warn};

use super::BtPeerMessageHandler;

impl BtPeerMessageHandler {
    // ── Fast Extension Message Handlers ──────────────────────────────────

    /// Handle receiving a Reject message (ID=13).
    ///
    /// Removes the matching outstanding request slot from the dispatcher.
    /// Returns an error if fast extension is not enabled (per spec, Reject
    /// is only valid with fast extension).
    ///
    /// Mirrors C++ `BtRejectMessage::doReceivedAction()`.
    pub fn on_reject_received(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> std::result::Result<(), String> {
        if !self.fast_extension_enabled {
            let msg = format!(
                "Reject message received but fast extension is not enabled (piece={}, begin={}, len={})",
                index, begin, length
            );
            warn!("PeerHandler: {}", msg);
            return Err(msg);
        }

        let removed = self.dispatcher.remove_request_slot(index, begin, length);
        if removed {
            debug!(
                "PeerHandler: Reject received, removed outstanding request (piece={}, begin={})",
                index, begin
            );
        } else {
            trace!(
                "PeerHandler: Reject received but no matching outstanding request (piece={}, begin={})",
                index, begin
            );
        }
        Ok(())
    }

    /// Handle receiving an AllowedFast message (ID=11).
    ///
    /// Adds the piece index to the peer's allowed-fast set.
    /// Returns an error if fast extension is not enabled.
    ///
    /// Mirrors C++ `BtAllowedFastMessage::doReceivedAction()`.
    pub fn on_allowed_fast_received(&mut self, index: u32) -> std::result::Result<(), String> {
        if !self.fast_extension_enabled {
            let msg = format!(
                "AllowedFast message received but fast extension is not enabled (piece={})",
                index
            );
            warn!("PeerHandler: {}", msg);
            return Err(msg);
        }

        self.peer_allowed_fast_set.insert(index);
        trace!(
            "PeerHandler: AllowedFast received for piece {} (set size={})",
            index,
            self.peer_allowed_fast_set.len()
        );
        Ok(())
    }

    /// Handle receiving a SuggestPiece message (ID=12).
    ///
    /// Currently a no-op — the C++ implementation also ignores this message
    /// (TODO in original code). May be used in the future for piece priority
    /// boosting.
    ///
    /// Mirrors C++ `BtSuggestPieceMessage::doReceivedAction()`.
    pub fn on_suggest_received(&mut self, index: u32) {
        trace!(
            "PeerHandler: SuggestPiece received for piece {} (currently ignored)",
            index
        );
    }

    /// Handle receiving a Port message (ID=9).
    ///
    /// If DHT is enabled and the port is non-zero, the caller should create
    /// a DHT node and ping it. If bootstrap is needed, the caller should
    /// initiate a node_lookup task.
    ///
    /// This handler only logs the event; actual DHT operations are delegated
    /// to the caller.
    ///
    /// Mirrors C++ `BtPortMessage::doReceivedAction()`.
    pub fn on_port_received(&mut self, port: u16) {
        if port != 0 {
            trace!(
                "PeerHandler: Port received (port={}), DHT action delegated to caller",
                port
            );
        } else {
            trace!("PeerHandler: Port received (port=0), ignoring");
        }
    }

    /// Handle receiving an Extended message (ID=20).
    ///
    /// Delegates to the extension message handler. This handler only logs
    /// the event; actual processing is delegated to the caller.
    ///
    /// Mirrors C++ `BtExtendedMessage::doReceivedAction()` which calls
    /// `extensionMessage->doReceivedAction()`.
    pub fn on_extended_received(&mut self, ext_id: u8, payload: &[u8]) {
        trace!(
            "PeerHandler: Extended message received (ext_id={}, payload_len={})",
            ext_id,
            payload.len()
        );
    }

    // ── Fast Extension & Metadata State Accessors ───────────────────────

    /// Check if fast extension is enabled for this peer.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.fast_extension_enabled
    }

    /// Enable or disable the fast extension for this peer.
    ///
    /// Should be called once when the handshake completes and the fast
    /// extension bit is set in the reserved bytes.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        self.fast_extension_enabled = enabled;
        debug!("PeerHandler: fast extension set to {}", enabled);
    }

    /// Set metadata-get mode.
    ///
    /// When true, certain side effects (e.g., bitfield updates) are skipped
    /// because we only need metadata, not actual piece data.
    pub fn set_metadata_get_mode(&mut self, mode: bool) {
        self.metadata_get_mode = mode;
        debug!("PeerHandler: metadata_get_mode set to {}", mode);
    }

    /// Check if metadata-get mode is active.
    pub fn is_metadata_get_mode(&self) -> bool {
        self.metadata_get_mode
    }

    /// Check if a piece index is in the peer's allowed-fast set.
    pub fn is_in_peer_allowed_fast(&self, index: u32) -> bool {
        self.peer_allowed_fast_set.contains(&index)
    }
}
