use std::collections::HashMap;
use tracing::{debug, info};

/// Tracks duplicate requests during endgame mode.
///
/// In endgame mode (when <=5 pieces remain incomplete), we request the same block
/// from multiple peers simultaneously to speed up completion. When any peer responds
/// with the block data, we send Cancel messages to the other peers that also received
/// the request for that block.
///
/// This struct maintains the mapping from block identifiers to the list of peers
/// that were sent duplicate requests, enabling efficient cancellation on arrival.
pub struct EndgameState {
    /// Map from (piece_index, offset, length) -> list of peer indices that received this request
    active_duplicate_requests: HashMap<(u32, u32, u32), Vec<usize>>,
    /// Whether we're currently in endgame mode
    active: bool,
}

impl EndgameState {
    /// Create a new EndgameState in inactive state
    pub fn new() -> Self {
        Self {
            active_duplicate_requests: HashMap::new(),
            active: false,
        }
    }

    /// Enter endgame mode - enables duplicate request tracking
    pub fn enter_endgame(&mut self) {
        if !self.active {
            self.active = true;
            info!("[BT] === Entering endgame mode ===");
        }
    }

    /// Exit endgame mode and clear all tracked requests
    pub fn exit_endgame(&mut self) {
        if self.active {
            self.active = false;
            self.active_duplicate_requests.clear();
            debug!(
                "[BT] Exiting endgame mode, cleared {} tracked requests",
                self.active_duplicate_requests.len()
            );
        }
    }

    /// Register that a request was sent to a peer during endgame
    ///
    /// This tracks which peers have pending requests for each block so we can
    /// cancel redundant requests when the first response arrives.
    pub fn track_request(&mut self, piece: u32, offset: u32, len: u32, peer_id: usize) {
        let key = (piece, offset, len);
        self.active_duplicate_requests
            .entry(key)
            .or_default()
            .push(peer_id);
    }

    /// When a block arrives, find other peers that have pending requests for the same block
    ///
    /// Returns the list of peer indices that should receive Cancel messages.
    /// Does NOT remove the entry (call remove_request after sending cancels).
    pub fn get_cancel_targets(&self, piece: u32, offset: u32, len: u32) -> Vec<usize> {
        let key = (piece, offset, len);
        self.active_duplicate_requests
            .get(&key)
            .map(|peers| peers.to_vec())
            .unwrap_or_default()
    }

    /// Remove a tracked request after cancel or completion
    ///
    /// Called after Cancel messages have been sent and the block is fully processed.
    pub fn remove_request(&mut self, piece: u32, offset: u32, len: u32) {
        let key = (piece, offset, len);
        self.active_duplicate_requests.remove(&key);
    }

    /// Check if endgame mode is currently active
    pub fn is_endgame_active(&self) -> bool {
        self.active
    }

    /// Get the number of actively tracked duplicate requests (for debugging/metrics)
    #[allow(dead_code)] // Debugging metric; used in tests only
    pub fn tracked_count(&self) -> usize {
        self.active_duplicate_requests.len()
    }
}

impl Default for EndgameState {
    fn default() -> Self {
        Self::new()
    }
}
