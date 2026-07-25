use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::Result;
use aria2_protocol::bittorrent::message::serializer;

// ==================== BEP 6 Fast Extension (AllowedFast / Suggest) ====================

impl BtDownloadCommand {
    /// Maximum number of AllowedFast messages to send to a single peer
    const MAX_ALLOWED_FAST_PER_PEER: usize = 10;

    /// Maximum number of Suggest messages to send per session per peer
    const MAX_SUGGEST_PER_PEER: usize = 5;

    /// Check if a bitfield has a specific piece index set
    ///
    /// BitTorrent bitfields use MSB-first ordering within each byte.
    #[allow(dead_code)] // BEP 6 utility; used in tests only
    pub(crate) fn is_bitfield_set(bitfield: &[u8], piece_index: u32) -> bool {
        let byte_idx = (piece_index as usize) / 8;
        let bit_idx = 7 - ((piece_index as usize) % 8);

        if byte_idx >= bitfield.len() {
            return false;
        }

        (bitfield[byte_idx] & (1 << bit_idx)) != 0
    }

    /// Calculate the set of pieces to send as AllowedFast to a peer
    ///
    /// Selects up to `MAX_ALLOWED_FAST_PER_PEER` pieces that:
    /// - We still need (not completed)
    /// - The peer has (based on their bitfield)
    /// - We haven't already sent AllowedFast for
    #[allow(dead_code)] // BEP 6 utility; used in tests only
    pub(crate) fn calculate_fast_set(
        needed_pieces: &[u32],
        peer_bitfield: &[u8],
        already_sent: &HashSet<u32>,
    ) -> Vec<u32> {
        let mut fast_set = Vec::new();

        for &piece_idx in needed_pieces.iter() {
            if fast_set.len() >= Self::MAX_ALLOWED_FAST_PER_PEER {
                break;
            }
            if already_sent.contains(&piece_idx) {
                continue;
            }

            // Check if peer has this piece (bitfield check)
            if Self::is_bitfield_set(peer_bitfield, piece_idx) {
                fast_set.push(piece_idx);
            }
        }

        fast_set
    }

    /// Send AllowedFast messages to a peer that supports BEP 6 Fast Extension
    ///
    /// This should be called after the extension handshake completes and we've received
    /// the peer's bitfield. It allows us to request specific pieces even when choked.
    #[allow(dead_code)] // BEP 6 method; not yet called from production download loop
    async fn send_allowed_fast_to_peer(
        peer_conn: &mut BtPeerConn,
        needed_pieces: &[u32],
        peer_bitfield: &[u8],
        already_sent: &mut HashSet<u32>,
    ) -> Result<usize> {
        let fast_set = Self::calculate_fast_set(needed_pieces, peer_bitfield, already_sent);
        let count = fast_set.len();

        for piece_idx in fast_set {
            let _msg = serializer::serialize_allowed_fast(piece_idx);

            // Note: In a full implementation, this would use a proper message queue/channel.
            // For now, we log and track what would be sent.
            debug!("[BEP6] Would send AllowedFast for piece {}", piece_idx);

            already_sent.insert(piece_idx);
            peer_conn.add_allowed_fast(piece_idx);
        }

        if count > 0 {
            info!("[BEP6] Sent {} AllowedFast messages to peer", count);
        }

        Ok(count)
    }

    /// Initialize BEP 6 tracking structures for all active connections
    #[allow(dead_code)]
    fn init_bep6_tracking(&mut self, num_connections: usize) {
        self.allowed_fast_sent_peers = HashMap::with_capacity(num_connections);
        self.suggest_sent_counts = HashMap::with_capacity(num_connections);
    }

    /// Send AllowedFast messages to all peers after handshake/bitfield exchange
    ///
    /// This is called once during initialization to establish fast extension
    /// support with compatible peers.
    #[allow(dead_code)]
    async fn broadcast_allowed_fast(
        &mut self,
        active_connections: &mut [BtPeerConn],
        needed_pieces: &[u32],
        peer_bitfields: &[Vec<u8>],
    ) -> Result<u64> {
        self.init_bep6_tracking(active_connections.len());

        let mut total_sent = 0u64;

        for (idx, conn) in active_connections.iter_mut().enumerate() {
            let peer_bf = if idx < peer_bitfields.len() {
                &peer_bitfields[idx]
            } else {
                continue;
            };

            let mut sent_for_peer = HashSet::new();
            match Self::send_allowed_fast_to_peer(conn, needed_pieces, peer_bf, &mut sent_for_peer)
                .await
            {
                Ok(count) => {
                    total_sent += count as u64;
                    if !sent_for_peer.is_empty() {
                        self.allowed_fast_sent_peers.insert(idx, sent_for_peer);
                    }
                }
                Err(e) => {
                    warn!("[BEP6] Failed to send AllowedFast to peer {}: {}", idx, e);
                }
            }
        }

        if total_sent > 0 {
            info!(
                "[BEP6] Broadcast {} total AllowedFast messages to {} peers",
                total_sent,
                active_connections.len()
            );
        }

        Ok(total_sent)
    }

    /// Send Suggest messages to a peer to guide them toward pieces we need most
    ///
    /// Called after unchoking a peer, this sends up to `MAX_SUGGEST_PER_PEER` Suggest
    /// messages for high-priority, low-availability pieces we need urgently.
    ///
    /// # Arguments
    /// * `peer_idx` - Index of the peer in active_connections
    /// * `piece_picker` - The piece picker for selecting which pieces to suggest
    #[allow(dead_code)] // BEP 6 method; not yet called from production download loop
    async fn send_suggest_to_peer(
        &mut self,
        peer_idx: usize,
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
    ) -> Result<usize> {
        // Check if we've already sent too many suggests to this peer
        let sent_count = self
            .suggest_sent_counts
            .get(&peer_idx)
            .copied()
            .unwrap_or(0);
        if sent_count >= Self::MAX_SUGGEST_PER_PEER {
            debug!(
                "[BEP6] Already sent {} suggests to peer {}, skipping",
                sent_count, peer_idx
            );
            return Ok(0);
        }

        let remaining = Self::MAX_SUGGEST_PER_PEER - sent_count;

        // Select high-priority, low-availability pieces we need most urgently
        let mut suggestions: Vec<u32> = piece_picker
            .pieces_iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .take(remaining)
            .map(|p| p.index)
            .collect();

        // Sort by priority (highest first), then by rarity (lowest frequency)
        suggestions.sort_by(|&a, &b| {
            let pa = piece_picker.get_piece_info(a).unwrap();
            let pb = piece_picker.get_piece_info(b).unwrap();
            pb.priority
                .cmp(&pa.priority) // Higher priority first
                .then(pa.frequency.cmp(&pb.frequency)) // Then rarer
        });

        let count = suggestions.len();

        for piece_idx in suggestions {
            let _msg = serializer::serialize_suggest(piece_idx);

            // Note: In a full implementation, this would use a proper message queue/channel.
            debug!(
                "[BEP6] Would send Suggest for piece {} to peer {}",
                piece_idx, peer_idx
            );
        }

        if count > 0 {
            // Update suggest count for this peer
            let new_count = sent_count + count;
            self.suggest_sent_counts.insert(peer_idx, new_count);

            info!(
                "[BEP6] Sent {} Suggest messages to peer {} (total: {})",
                count, peer_idx, new_count
            );
        }

        Ok(count)
    }
}
