//! Session resource lifecycle, bitfield delegation, fast extension, and
//! AllowedFast (BEP 6) methods for [`BtPeerConn`].

use std::collections::HashSet;

use super::super::session_resource::PeerSessionResource;
use super::super::types::ConnectionType;
use super::BtPeerConn;

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Connection classification
    // -----------------------------------------------------------------------

    /// Get the connection type.
    pub fn connection_type(&self) -> ConnectionType {
        self.connection_type
    }

    /// Check if this is a uTP connection.
    pub fn is_utp(&self) -> bool {
        self.connection_type == ConnectionType::Utp
    }

    // -----------------------------------------------------------------------
    // AllowedFast (BEP 6)
    // -----------------------------------------------------------------------

    /// Add a piece index to the AllowedFast set.
    ///
    /// Called when an AllowedFast message is received from this peer.
    /// Pieces in the allowed_fast set can be requested even when the peer
    /// is choked (BEP 6 / Fast Extension).
    pub fn add_allowed_fast(&mut self, index: u32) {
        self.allowed_fast.insert(index);
    }

    /// Check whether a piece index is in the AllowedFast set.
    ///
    /// Returns true if the peer has granted fast access to this piece,
    /// meaning a Request can be sent even while the peer is choked.
    pub fn is_allowed_fast(&self, index: u32) -> bool {
        self.allowed_fast.contains(&index)
    }

    /// Get a reference to the full AllowedFast set.
    ///
    /// Returns all piece indices that this peer has allowed us to request
    /// via BEP 6 Fast Extension, even when choked.
    pub fn allowed_fast_set(&self) -> &HashSet<u32> {
        &self.allowed_fast
    }

    // -----------------------------------------------------------------------
    // Fast Extension delegation
    // -----------------------------------------------------------------------

    /// Check whether fast extension is enabled for this connection.
    ///
    /// Delegates to the session resource's `is_fast_extension_enabled()`.
    /// Returns `false` if no session resource is allocated.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.session_resource
            .as_ref()
            .is_some_and(|sr| sr.is_fast_extension_enabled())
    }

    /// Enable or disable fast extension for this connection.
    ///
    /// Delegates to the session resource's `set_fast_extension_enabled()`.
    /// Does nothing if no session resource is allocated.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        if let Some(sr) = &mut self.session_resource {
            sr.set_fast_extension_enabled(enabled);
        }
    }

    // -----------------------------------------------------------------------
    // Session resource lifecycle
    // -----------------------------------------------------------------------

    /// Allocate a [`PeerSessionResource`] for this connection.
    ///
    /// Called when the peer becomes active (after successful handshake).
    /// Does nothing if a session resource is already allocated.
    pub fn allocate_session_resource(&mut self, piece_length: u32, total_length: u64) {
        if self.session_resource.is_none() {
            self.session_resource = Some(PeerSessionResource::new(piece_length, total_length));
        }
    }

    /// Release the [`PeerSessionResource`], dropping all per-session state.
    pub fn release_session_resource(&mut self) {
        self.session_resource = None;
    }

    /// Reconfigure the session resource for new torrent parameters.
    ///
    /// No-op if no session resource is allocated.
    pub fn reconfigure_session_resource(&mut self, piece_length: u32, total_length: u64) {
        if let Some(ref mut res) = self.session_resource {
            res.reconfigure(piece_length, total_length);
        }
    }

    /// Check whether this connection has an active session resource.
    pub fn is_active(&self) -> bool {
        self.session_resource.is_some()
    }

    // -----------------------------------------------------------------------
    // Extension negotiation delegation
    // -----------------------------------------------------------------------

    /// Return the extension ID assigned by this peer to `name`.
    pub fn peer_extension_id(&self, name: &str) -> Option<u8> {
        self.session_resource
            .as_ref()
            .and_then(|resource| resource.get_extension_message_id(name))
    }

    /// Record an extension ID assigned by this peer during BEP 10 negotiation.
    pub fn register_peer_extension(&mut self, name: &str, id: u8) {
        if let Some(resource) = &mut self.session_resource {
            resource.add_extension(name, id);
        }
    }

    // -----------------------------------------------------------------------
    // Bitfield delegation (convenience methods)
    // -----------------------------------------------------------------------

    /// Check whether the peer has a given piece.
    ///
    /// Delegates to [`PeerSessionResource::has_piece`]. Returns `false` if
    /// no session resource is allocated.
    pub fn has_piece(&self, index: usize) -> bool {
        self.session_resource
            .as_ref()
            .is_some_and(|r| r.has_piece(index))
    }

    /// Set the peer bitfield from raw bytes.
    ///
    /// Delegates to [`PeerSessionResource::set_bitfield`]. No-op if no
    /// session resource is allocated.
    pub fn set_peer_bitfield(&mut self, bitfield: &[u8]) {
        if let Some(ref mut res) = self.session_resource {
            res.set_bitfield(bitfield);
        }
    }

    /// Update the peer bitfield: set (operation=1) or clear (operation=0)
    /// the bit at `index`.
    ///
    /// Delegates to [`PeerSessionResource::update_bitfield`]. No-op if no
    /// session resource is allocated.
    pub fn update_peer_bitfield(&mut self, index: usize, operation: i32) {
        if let Some(ref mut res) = self.session_resource {
            res.update_bitfield(index, operation);
        }
    }

    /// Mark the peer as a seeder (has all pieces).
    ///
    /// Delegates to [`PeerSessionResource::mark_seeder`]. No-op if no
    /// session resource is allocated.
    pub fn mark_seeder(&mut self) {
        self.seeder = true;
        if let Some(ref mut res) = self.session_resource {
            res.mark_seeder();
        }
    }
}
