//! Per-session resource for an active BitTorrent peer connection.
//!
//! Mirrors the C++ `PeerSessionResource`. Allocated when a peer session starts
//! and released when it ends. Contains bitfield management, extension
//! negotiation, and message validation state.

use std::collections::HashMap;

use crate::engine::bt_message_validation::{BtMessageValidationError, BtMessageValidator};
use crate::segment::bitfield_util;

/// Per-session resource for an active BitTorrent peer connection.
///
/// Mirrors the C++ `PeerSessionResource`. Allocated when a peer session starts
/// and released when it ends. Contains bitfield management, extension
/// negotiation, and message validation state.
pub struct PeerSessionResource {
    /// Bitfield tracking which pieces this peer has.
    bitfield: Vec<u8>,
    /// Bitfield length in bytes.
    bitfield_length: usize,
    /// Piece length for the torrent.
    piece_length: u32,
    /// Total length of the torrent.
    total_length: u64,
    /// Number of pieces in the torrent.
    num_pieces: u32,
    /// Domain validator reused by the connection read path.
    message_validator: BtMessageValidator,

    // Fast Extension (BEP 6)
    /// Whether fast extension is enabled for this peer.
    fast_extension_enabled: bool,

    // Extension Protocol (BEP 10)
    /// Extension IDs negotiated with this peer: name -> message ID.
    peer_extensions: HashMap<Box<str>, u8>,
}

impl PeerSessionResource {
    /// Create a new `PeerSessionResource` for the explicit torrent geometry.
    ///
    /// `num_pieces` is authoritative because BEP 52 v2 multi-file torrents
    /// can have aligned piece space larger than their content length.
    pub fn new(piece_length: u32, num_pieces: u32, total_length: u64) -> Self {
        let bitfield_length = (num_pieces as usize).div_ceil(8);

        Self {
            bitfield: vec![0u8; bitfield_length],
            bitfield_length,
            piece_length,
            total_length,
            num_pieces,
            message_validator: BtMessageValidator::new(num_pieces, piece_length),
            fast_extension_enabled: false,
            peer_extensions: HashMap::new(),
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
    pub fn set_bitfield(&mut self, bitfield: &[u8]) -> Vec<u8> {
        let old = self.bitfield.clone();
        let copy_len = std::cmp::min(bitfield.len(), self.bitfield.len());
        self.bitfield[..copy_len].copy_from_slice(&bitfield[..copy_len]);
        // Zero-fill remaining bytes if source is shorter
        self.bitfield[copy_len..].fill(0);
        old
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
    fn set_all_bitfield(&mut self) {
        self.bitfield.fill(0xFF);
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

    /// Return the bitfield length in bytes.
    pub fn bitfield_length(&self) -> usize {
        self.bitfield_length
    }

    /// Reconfigure the session resource for new torrent geometry.
    ///
    /// Called when the torrent metadata is updated (e.g., after magnet
    /// link metadata exchange).
    pub fn reconfigure(&mut self, piece_length: u32, num_pieces: u32, total_length: u64) {
        let bitfield_length = (num_pieces as usize).div_ceil(8);

        self.bitfield.resize(bitfield_length, 0);
        self.bitfield_length = bitfield_length;
        self.piece_length = piece_length;
        self.total_length = total_length;
        self.num_pieces = num_pieces;
        self.message_validator = BtMessageValidator::new(num_pieces, piece_length);
    }

    /// Validate a parsed peer message against this torrent's geometry.
    pub(crate) fn validate_message(
        &self,
        message: &aria2_protocol::bittorrent::message::types::BtMessage,
    ) -> Result<(), BtMessageValidationError> {
        self.message_validator.validate(message)
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

    // -----------------------------------------------------------------------
    // Extension Protocol (BEP 10)
    // -----------------------------------------------------------------------

    /// Register an extension with the given key and message ID.
    pub fn add_extension(&mut self, key: &str, id: u8) {
        self.peer_extensions.insert(key.into(), id);
    }

    /// Look up the message ID for a given extension key.
    pub fn get_extension_message_id(&self, key: &str) -> Option<u8> {
        self.peer_extensions.get(key).copied()
    }
}
