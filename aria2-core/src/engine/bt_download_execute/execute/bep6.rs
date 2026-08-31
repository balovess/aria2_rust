//! BEP 6 Fast Extension (AllowedFast / Suggest / HaveAll / HaveNone) wiring.
//!
//! Implements the production send/receive cycle for BEP 6 messages:
//! - **AllowedFast**: Advertise pieces the peer can request even when choked.
//!   Sent after handshake/bitfield exchange, calculated using the BEP 6
//!   fast-set algorithm based on the peer's IP address.
//! - **Suggest**: Tell the peer which pieces we'd like them to request,
//!   sent after we unchoke the peer.
//! - **HaveAll / HaveNone**: Optimized bitfield alternatives sent during
//!   post-handshake when we have all or no pieces.
//!
//! Inbound BEP 6 messages (AllowedFast, Suggest, HaveAll, HaveNone,
//! Reject) are handled in `BtMessageHandler::wait_for_piece_block` and
//! `BtPeerInteractive::dispatch_message`.

use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

use super::super::types::PeerKey;
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

    /// Send the canonical BEP 6 fast set for a torrent to one peer.
    ///
    /// BEP 6 derives the set from the peer address, piece count and info hash;
    /// it does not depend on the peer bitfield or our current piece picker.
    pub async fn send_allowed_fast_for_torrent(
        peer_conn: &mut BtPeerConn,
        num_pieces: u32,
        info_hash: &[u8; 20],
        already_sent: &mut HashSet<u32>,
    ) -> Result<usize> {
        let fast_set = aria2_protocol::bittorrent::fast_set::compute_fast_set(
            &peer_conn.ip_addr,
            num_pieces,
            info_hash,
            Self::MAX_ALLOWED_FAST_PER_PEER,
        );
        let new_pieces: Vec<_> = fast_set
            .into_iter()
            .filter(|piece_idx| already_sent.insert(*piece_idx))
            .collect();
        let count: usize = new_pieces.len();
        if count == 0 {
            return Ok(0);
        }

        for piece_idx in new_pieces {
            peer_conn.queue_message(serializer::serialize_allowed_fast(piece_idx));
            peer_conn.add_allowed_fast(piece_idx);
        }
        peer_conn.flush_send_buffer().await?;
        Ok(count)
    }

    /// Compatibility helper for callers that intentionally provide a filtered
    /// piece set. New torrent setup code should use
    /// [`Self::send_allowed_fast_for_torrent`].
    pub async fn send_allowed_fast_to_peer(
        peer_conn: &mut BtPeerConn,
        needed_pieces: &[u32],
        peer_bitfield: &[u8],
        already_sent: &mut HashSet<u32>,
    ) -> Result<usize> {
        let fast_set = Self::calculate_fast_set(needed_pieces, peer_bitfield, already_sent);
        let count = fast_set.len();

        for piece_idx in fast_set {
            let msg_bytes = serializer::serialize_allowed_fast(piece_idx);
            peer_conn.queue_message(msg_bytes);
            already_sent.insert(piece_idx);
            peer_conn.add_allowed_fast(piece_idx);

            debug!("[BEP6] Queued AllowedFast for piece {}", piece_idx);
        }

        if count > 0 {
            peer_conn.flush_send_buffer().await?;
            info!("[BEP6] Sent {} AllowedFast messages to peer", count);
        }

        Ok(count)
    }

    /// Initialize BEP 6 tracking structures for all active connections
    pub(crate) fn init_bep6_tracking(&mut self, num_connections: usize) {
        self.allowed_fast_sent_peers = HashMap::with_capacity(num_connections);
        self.suggest_sent_counts = HashMap::with_capacity(num_connections);
    }

    /// Send AllowedFast messages to all peers after handshake/bitfield exchange.
    ///
    /// This is called once during initialization to establish fast extension
    /// support with compatible peers. Only sends to peers that have fast
    /// extension enabled (detected via handshake reserved bytes).
    pub async fn broadcast_allowed_fast(
        &mut self,
        active_connections: &mut [BtPeerConn],
        needed_pieces: &[u32],
        peer_bitfields: &[Vec<u8>],
    ) -> Result<u64> {
        self.init_bep6_tracking(active_connections.len());

        let mut total_sent = 0u64;

        for (idx, conn) in active_connections.iter_mut().enumerate() {
            // Only send AllowedFast if the peer supports BEP 6
            if !conn.is_fast_extension_enabled() {
                continue;
            }

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
                    if !sent_for_peer.is_empty()
                        && let Some(peer_key) = PeerKey::from_peer(&conn.ip_addr, conn.port)
                    {
                        self.allowed_fast_sent_peers.insert(peer_key, sent_for_peer);
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

    /// Send Suggest messages to a peer to guide them toward pieces we need most.
    ///
    /// Called after unchoking a peer, this sends up to `MAX_SUGGEST_PER_PEER`
    /// Suggest messages for high-priority, low-availability pieces.
    ///
    /// The messages are serialized and queued into the peer's send buffer.
    /// The caller should flush the buffer after calling this method.
    pub async fn send_suggest_to_peer(
        &mut self,
        peer_key: PeerKey,
        piece_picker: &crate::engine::bt_piece::PiecePicker,
        conn: &mut BtPeerConn,
    ) -> Result<usize> {
        // Check if we've already sent too many suggests to this peer
        let sent_count = self
            .suggest_sent_counts
            .get(&peer_key)
            .copied()
            .unwrap_or(0);
        if sent_count >= Self::MAX_SUGGEST_PER_PEER {
            debug!(
                "[BEP6] Already sent {} suggests to peer {}, skipping",
                sent_count,
                peer_key.address()
            );
            return Ok(0);
        }

        let remaining = Self::MAX_SUGGEST_PER_PEER - sent_count;

        // Select high-priority, low-availability pieces we need most urgently
        let mut suggestions: Vec<u32> = piece_picker
            .pieces_iter()
            .filter(|p| {
                piece_picker.is_allowed(p.index)
                    && !p.completed
                    && !p.in_progress
                    && p.frequency > 0
            })
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
            let msg_bytes = serializer::serialize_suggest(piece_idx);
            conn.queue_message(msg_bytes);
            debug!(
                "[BEP6] Queued Suggest for piece {} to peer {}",
                piece_idx,
                peer_key.address()
            );
        }

        if count > 0 {
            conn.flush_send_buffer().await?;

            // Update suggest count for this peer
            let new_count = sent_count + count;
            self.suggest_sent_counts.insert(peer_key, new_count);

            info!(
                "[BEP6] Sent {} Suggest messages to peer {} (total: {})",
                count,
                peer_key.address(),
                new_count
            );
        }

        Ok(count)
    }

    /// Send a HaveAll message to a peer (BEP 6 Fast Extension).
    ///
    /// Used as an optimized alternative to sending a full Bitfield message
    /// when we have all pieces. Only valid when fast extension is enabled.
    pub async fn send_have_all_to_peer(conn: &mut BtPeerConn) -> Result<()> {
        let msg_bytes = serializer::serialize_have_all();
        conn.queue_message(msg_bytes);
        conn.flush_send_buffer().await?;
        debug!("[BEP6] Sent HaveAll to peer");
        Ok(())
    }

    /// Send a HaveNone message to a peer (BEP 6 Fast Extension).
    ///
    /// Used as an optimized alternative to sending a full Bitfield message
    /// when we have no pieces. Only valid when fast extension is enabled.
    pub async fn send_have_none_to_peer(conn: &mut BtPeerConn) -> Result<()> {
        let msg_bytes = serializer::serialize_have_none();
        conn.queue_message(msg_bytes);
        conn.flush_send_buffer().await?;
        debug!("[BEP6] Sent HaveNone to peer");
        Ok(())
    }
}
