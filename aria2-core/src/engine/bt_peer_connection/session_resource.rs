//! Per-session resource for an active BitTorrent peer connection.
//!
//! Mirrors the C++ `PeerSessionResource`. Allocated when a peer session starts
//! and released when it ends. Contains bitfield management, extension support,
//! and choking algorithm integration fields.

use std::collections::{HashMap, HashSet};

use crate::segment::bitfield_util;

/// Per-session resource for an active BitTorrent peer connection.
///
/// Mirrors the C++ `PeerSessionResource`. Allocated when a peer session starts
/// and released when it ends. Contains bitfield management, extension support,
/// and choking algorithm integration fields.
pub struct PeerSessionResource {
    /// Bitfield tracking which pieces this peer has.
    bitfield: Vec<u8>,
    /// Bitfield length in bytes.
    pub bitfield_length: usize,
    /// Piece length for the torrent.
    piece_length: u32,
    /// Total length of the torrent.
    total_length: u64,
    /// Number of pieces in the torrent.
    num_pieces: u32,

    // Fast Extension (BEP 6)
    /// Whether fast extension is enabled for this peer.
    fast_extension_enabled: bool,
    /// Piece indices that the peer has allowed us to request (even when choked).
    peer_allowed_index_set: HashSet<u32>,
    /// Piece indices that we have allowed the peer to request (even when choked).
    am_allowed_index_set: HashSet<u32>,

    // Extension Protocol (BEP 10)
    /// Whether extended messaging is enabled.
    extended_messaging_enabled: bool,
    /// Extension message registry: key -> message ID.
    extension_registry: HashMap<String, u8>,

    // DHT (BEP 5)
    /// Whether DHT is enabled for this peer.
    dht_enabled: bool,

    // Choking Algorithm Integration
    /// Whether choking this peer is required (set by choking algorithm).
    choking_required: bool,
    /// Whether this peer is eligible for optimistic unchoking.
    opt_unchoking: bool,
    /// Whether this peer is snubbing (not sending data despite being unchoked).
    snubbing: bool,
}

impl PeerSessionResource {
    /// Create a new `PeerSessionResource` for a torrent with the given
    /// piece length and total length.
    pub fn new(piece_length: u32, total_length: u64) -> Self {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            total_length.div_ceil(piece_length as u64) as u32
        };
        let bitfield_length = (num_pieces as usize).div_ceil(8);

        Self {
            bitfield: vec![0u8; bitfield_length],
            bitfield_length,
            piece_length,
            total_length,
            num_pieces,
            fast_extension_enabled: false,
            peer_allowed_index_set: HashSet::new(),
            am_allowed_index_set: HashSet::new(),
            extended_messaging_enabled: false,
            extension_registry: HashMap::new(),
            dht_enabled: false,
            choking_required: true,
            opt_unchoking: false,
            snubbing: false,
        }
    }

    // -----------------------------------------------------------------------
    // Bitfield
    // -----------------------------------------------------------------------

    /// Check whether the peer has a given piece.
    ///
    /// Returns `false` if the index is out of range or the bitfield is
    /// too short.
    pub fn has_piece(&self, index: usize) -> bool {
        bitfield_util::test_bit(&self.bitfield, self.num_pieces as usize, index)
    }

    /// Set the peer bitfield from raw bytes.
    ///
    /// Copies `bitfield` into the internal storage, truncating or
    /// zero-extending as needed.
    pub fn set_bitfield(&mut self, bitfield: &[u8]) {
        let copy_len = std::cmp::min(bitfield.len(), self.bitfield.len());
        self.bitfield[..copy_len].copy_from_slice(&bitfield[..copy_len]);
        // Zero-fill remaining bytes if source is shorter
        for byte in &mut self.bitfield[copy_len..] {
            *byte = 0;
        }
    }

    /// Update the peer bitfield: set (operation=1) or clear (operation=0)
    /// the bit at `index`.
    pub fn update_bitfield(&mut self, index: usize, operation: i32) {
        if index >= self.num_pieces as usize {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        if byte >= self.bitfield.len() {
            return;
        }
        if operation == 1 {
            self.bitfield[byte] |= 1 << bit;
        } else {
            self.bitfield[byte] &= !(1 << bit);
        }
    }

    /// Mark all pieces as available (seeder bitfield).
    pub fn set_all_bitfield(&mut self) {
        for byte in &mut self.bitfield {
            *byte = 0xFF;
        }
        // Clear trailing bits beyond num_pieces
        let remaining = (self.num_pieces as usize) % 8;
        if remaining != 0 {
            let extra = 8 - remaining;
            if let Some(last) = self.bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Mark the peer as a seeder (has all pieces).
    pub fn mark_seeder(&mut self) {
        self.set_all_bitfield();
    }

    /// Clear the entire bitfield (peer has no pieces).
    ///
    /// Used after receiving a HaveNone message (BEP 6) to reset the
    /// peer's piece availability.
    /// Mirrors C++ `BtHaveNoneMessage::doReceivedAction()`.
    pub fn clear_bitfield(&mut self) {
        for byte in &mut self.bitfield {
            *byte = 0;
        }
    }

    /// Check whether the peer is a seeder (has all pieces).
    pub fn is_seeder(&self) -> bool {
        // Count set bits and compare with num_pieces
        let mut count = 0usize;
        for &byte in &self.bitfield {
            count += byte.count_ones() as usize;
        }
        // Adjust for trailing bits
        let remaining = (self.num_pieces as usize) % 8;
        if remaining != 0 && !self.bitfield.is_empty() {
            let extra = 8 - remaining;
            if let Some(&last) = self.bitfield.last() {
                let trailing = (last & ((1u8 << extra) - 1)).count_ones() as usize;
                count -= trailing;
            }
        }
        count == self.num_pieces as usize
    }

    /// Get a reference to the raw bitfield bytes.
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    /// Reconfigure the session resource for a new piece/total length.
    ///
    /// Called when the torrent metadata is updated (e.g., after magnet
    /// link metadata exchange).
    pub fn reconfigure(&mut self, piece_length: u32, total_length: u64) {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            total_length.div_ceil(piece_length as u64) as u32
        };
        let bitfield_length = (num_pieces as usize).div_ceil(8);

        self.bitfield.resize(bitfield_length, 0);
        self.bitfield_length = bitfield_length;
        self.piece_length = piece_length;
        self.total_length = total_length;
        self.num_pieces = num_pieces;
    }

    /// Get the number of pieces.
    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    /// Get the piece length.
    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Get the total length.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    // -----------------------------------------------------------------------
    // Fast Extension (BEP 6)
    // -----------------------------------------------------------------------

    /// Enable or disable fast extension.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        self.fast_extension_enabled = enabled;
    }

    /// Check whether fast extension is enabled.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.fast_extension_enabled
    }

    /// Add a piece index to the set the peer has allowed us to request.
    pub fn add_peer_allowed_index(&mut self, index: u32) {
        self.peer_allowed_index_set.insert(index);
    }

    /// Check whether a piece index is in the peer-allowed set.
    pub fn is_in_peer_allowed_index_set(&self, index: u32) -> bool {
        self.peer_allowed_index_set.contains(&index)
    }

    /// Add a piece index to the set we have allowed the peer to request.
    pub fn add_am_allowed_index(&mut self, index: u32) {
        self.am_allowed_index_set.insert(index);
    }

    /// Check whether a piece index is in the am-allowed set.
    pub fn is_in_am_allowed_index_set(&self, index: u32) -> bool {
        self.am_allowed_index_set.contains(&index)
    }

    // -----------------------------------------------------------------------
    // Extension Protocol (BEP 10)
    // -----------------------------------------------------------------------

    /// Enable or disable extended messaging.
    pub fn set_extended_messaging_enabled(&mut self, enabled: bool) {
        self.extended_messaging_enabled = enabled;
    }

    /// Check whether extended messaging is enabled.
    pub fn is_extended_messaging_enabled(&self) -> bool {
        self.extended_messaging_enabled
    }

    /// Register an extension with the given key and message ID.
    pub fn add_extension(&mut self, key: &str, id: u8) {
        self.extension_registry.insert(key.to_string(), id);
    }

    /// Look up the message ID for a given extension key.
    pub fn get_extension_message_id(&self, key: &str) -> Option<u8> {
        self.extension_registry.get(key).copied()
    }

    /// Look up the extension name for a given message ID.
    pub fn get_extension_name(&self, id: u8) -> Option<&str> {
        self.extension_registry
            .iter()
            .find(|&(_, &v)| v == id)
            .map(|(k, _)| k.as_str())
    }

    // -----------------------------------------------------------------------
    // DHT (BEP 5)
    // -----------------------------------------------------------------------

    /// Enable or disable DHT for this peer.
    pub fn set_dht_enabled(&mut self, enabled: bool) {
        self.dht_enabled = enabled;
    }

    /// Check whether DHT is enabled.
    pub fn is_dht_enabled(&self) -> bool {
        self.dht_enabled
    }

    // -----------------------------------------------------------------------
    // Choking Algorithm Integration
    // -----------------------------------------------------------------------

    /// Set whether choking this peer is required.
    pub fn set_choking_required(&mut self, required: bool) {
        self.choking_required = required;
    }

    /// Check whether choking this peer is required.
    pub fn choking_required(&self) -> bool {
        self.choking_required
    }

    /// Set whether this peer is eligible for optimistic unchoking.
    pub fn set_opt_unchoking(&mut self, enabled: bool) {
        self.opt_unchoking = enabled;
    }

    /// Check whether this peer is eligible for optimistic unchoking.
    pub fn opt_unchoking(&self) -> bool {
        self.opt_unchoking
    }

    /// Set whether this peer is snubbing.
    pub fn set_snubbing(&mut self, snubbing: bool) {
        self.snubbing = snubbing;
    }

    /// Check whether this peer is snubbing.
    pub fn snubbing(&self) -> bool {
        self.snubbing
    }

    /// Determine whether this peer should be choked.
    ///
    /// Returns `true` if choking is required and the peer is not eligible
    /// for optimistic unchoking.
    pub fn should_be_choking(&self) -> bool {
        self.choking_required && !self.opt_unchoking
    }

    /// Count outstanding upload operations (placeholder).
    ///
    /// In the C++ code this counts pending upload requests. For now it
    /// returns 0; will be wired up when upload scheduling is implemented.
    pub fn count_outstanding_upload(&self) -> usize {
        0
    }
}
