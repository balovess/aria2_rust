//! Piece selection logic for generating BT Request messages.

use rand::seq::SliceRandom;
use tracing::trace;

use super::{BtRequestFactory, PieceBlockRequest};

impl BtRequestFactory {
    /// Create Request messages for missing blocks.
    ///
    /// Mirrors C++ `createRequestMessages(max, endGame)`.
    ///
    /// # Arguments
    ///
    /// * `max_count` — Maximum number of requests to generate.
    /// * `is_endgame` — If true, use endgame mode (request all missing blocks,
    ///   skip outstanding, shuffle order).
    /// * `is_outstanding` — Closure that returns true if a block already has
    ///   an outstanding request (used in endgame mode).
    ///
    /// # Returns
    ///
    /// A vector of `PieceBlockRequest` descriptors containing
    /// `(index, begin, length)` for each block to request.
    pub fn create_request_messages(
        &mut self,
        max_count: usize,
        is_endgame: bool,
        is_outstanding: impl Fn(u32, u32) -> bool,
    ) -> Vec<PieceBlockRequest> {
        if is_endgame {
            self.create_request_messages_on_end_game(max_count, &is_outstanding)
        } else {
            self.create_request_messages_normal(max_count)
        }
    }

    /// Normal mode: request missing+unused blocks.
    ///
    /// Mirrors C++ `createRequestMessages()` (non-endgame path).
    /// Iterates through target pieces, finds missing unused blocks,
    /// marks them as in-use, and generates request descriptors.
    fn create_request_messages_normal(&mut self, max_count: usize) -> Vec<PieceBlockRequest> {
        let mut requests = Vec::with_capacity(max_count);
        let mut remaining = max_count;

        for piece in &mut self.pieces {
            if remaining == 0 {
                break;
            }

            // Get missing unused block indexes (marks them as in-use)
            let block_indexes = piece.get_missing_unused_block_indexes(remaining);

            for block_index in &block_indexes {
                let begin = *block_index as u32 * piece.block_length();
                let length = piece.block_length_at(*block_index);

                trace!(
                    "BtRequestFactory: creating request index={}, begin={}, blockIndex={}",
                    piece.index(),
                    begin,
                    block_index
                );

                requests.push(PieceBlockRequest {
                    index: piece.index() as u32,
                    begin,
                    length,
                });
            }

            remaining -= block_indexes.len();
        }

        requests
    }

    /// Endgame mode: request all missing blocks, skip outstanding, shuffle.
    ///
    /// Mirrors C++ `createRequestMessagesOnEndGame()`.
    /// Iterates through target pieces, collects all missing block indexes
    /// (regardless of in-use status), shuffles them, then generates requests
    /// for blocks that don't already have an outstanding request.
    fn create_request_messages_on_end_game(
        &mut self,
        max_count: usize,
        is_outstanding: &impl Fn(u32, u32) -> bool,
    ) -> Vec<PieceBlockRequest> {
        let mut requests = Vec::with_capacity(max_count);
        let mut rng = rand::thread_rng();

        for piece in &self.pieces {
            if requests.len() >= max_count {
                break;
            }

            // Get all missing block indexes (using bitfield bytes)
            let bitfield_bytes = piece.missing_block_bitfield_bytes();
            let num_blocks = piece.count_blocks();

            // Decode the bitfield into block indexes
            let mut missing_block_indexes = Vec::new();
            let mut block_index: usize = 0;
            for &byte in &bitfield_bytes {
                let mut mask: u8 = 0x80; // MSB first
                for _ in 0..8 {
                    if block_index >= num_blocks {
                        break;
                    }
                    if byte & mask != 0 {
                        missing_block_indexes.push(block_index);
                    }
                    mask >>= 1;
                    block_index += 1;
                }
            }

            // Shuffle to distribute requests across peers
            missing_block_indexes.shuffle(&mut rng);

            for block_index in missing_block_indexes {
                if requests.len() >= max_count {
                    break;
                }

                // Skip blocks that already have an outstanding request
                if is_outstanding(piece.index() as u32, block_index as u32) {
                    continue;
                }

                let begin = block_index as u32 * piece.block_length();
                let length = piece.block_length_at(block_index);

                trace!(
                    "BtRequestFactory: creating endgame request index={}, begin={}, blockIndex={}",
                    piece.index(),
                    begin,
                    block_index
                );

                requests.push(PieceBlockRequest {
                    index: piece.index() as u32,
                    begin,
                    length,
                });
            }
        }

        requests
    }
}