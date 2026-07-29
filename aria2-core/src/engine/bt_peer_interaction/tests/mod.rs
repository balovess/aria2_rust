//! Tests for the BT peer interaction module.

mod choking_interest;
mod constants_and_types;
mod dispatch_message;
mod extension;
mod keepalive_flooding;
mod peer_id;
mod piece_exchange;
mod state_machine;

use std::time::{Duration, Instant};

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::segment::piece::Piece;

use super::piece_provider::PieceProvider;

/// Helper to create an `Instant` representing a point in the past.
/// Uses `checked_sub` to avoid panicking on platforms where `Instant`
/// origin is near zero (e.g., shortly after system boot on Windows).
#[allow(dead_code)]
pub(crate) fn instant_past(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(Instant::now())
}

/// Create a minimal `BtPeerConn` for testing purposes.
#[allow(dead_code)]
pub(crate) fn make_test_conn() -> BtPeerConn {
    BtPeerConn::new_stub(&[0u8; 20])
}

/// Mock piece provider that simulates PieceStorage operations.
#[allow(dead_code)]
pub(crate) struct MockPieceProvider {
    /// Whether has_missing_piece() returns true.
    pub has_missing: bool,
    /// Whether has_missing_unused_piece() returns true.
    pub has_missing_unused: bool,
    /// Whether is_end_game() returns true.
    pub is_end_game: bool,
    /// Whether enter_end_game() was called.
    pub entered_end_game: bool,
    /// Pieces to return from get_missing_pieces().
    pub missing_pieces: Vec<Piece>,
    /// Pieces to return from get_missing_fast_pieces().
    pub fast_pieces: Vec<Piece>,
}

#[allow(dead_code)]
impl MockPieceProvider {
    pub fn new() -> Self {
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
        self.missing_pieces
            .drain(..count.min(self.missing_pieces.len()))
            .collect()
    }

    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        _peer: &BtPeerConn,
        _target_piece_indexes: &[u32],
        _cuid: u64,
    ) -> Vec<Piece> {
        self.fast_pieces
            .drain(..count.min(self.fast_pieces.len()))
            .collect()
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
