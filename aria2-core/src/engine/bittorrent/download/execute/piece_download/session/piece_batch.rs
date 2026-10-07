use crate::engine::bittorrent::peer::message_handler::types::BLOCK_SIZE;

use super::PieceDownloadSession;

const MAX_BT_PIECES_IN_FLIGHT: usize = 8;
const MAX_BT_PIECE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

impl PieceDownloadSession<'_> {
    pub(super) fn normal_piece_batch_limit(&self, first_piece_index: usize) -> usize {
        if self.endgame_state.is_endgame_active() {
            return 1;
        }
        let piece_length = self.actual_piece_length(first_piece_index).max(1) as usize;
        let blocks_per_piece = piece_length.div_ceil(BLOCK_SIZE as usize).max(1);
        let aggregate_request_window = self
            .swarm
            .iter()
            .filter(|peer| !peer.dead)
            .map(|peer| peer.max_outstanding_requests)
            .sum::<usize>();
        let window_limited = aggregate_request_window.div_ceil(blocks_per_piece).max(1);
        let memory_limited = (MAX_BT_PIECE_BUFFER_BYTES / piece_length).max(1);
        window_limited
            .min(MAX_BT_PIECES_IN_FLIGHT)
            .min(memory_limited)
    }
}
