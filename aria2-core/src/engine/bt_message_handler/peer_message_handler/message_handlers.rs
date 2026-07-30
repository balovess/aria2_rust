//! Message side-effect handlers for standard BitTorrent messages.

use tracing::{debug, trace};

use super::super::types::{PeerStateUpdate, RequestResponse};
use super::BtPeerMessageHandler;

impl BtPeerMessageHandler {
    /// Handle receiving a Have message (ID=4).
    ///
    /// Returns [`PeerStateUpdate::HavePiece`] so the caller can update the
    /// peer's bitfield and piece stats. If the peer becomes a seeder and
    /// our download is finished, also returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtHaveMessage::doReceivedAction()`.
    pub fn on_have_received(
        &mut self,
        piece_index: u32,
        is_seeder_after: bool,
        download_finished: bool,
    ) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: Have received for piece {}", piece_index);

        let mut updates = vec![PeerStateUpdate::HavePiece { index: piece_index }];

        if is_seeder_after && download_finished {
            debug!(
                "PeerHandler: peer became seeder after Have({}) and download finished — disconnect",
                piece_index
            );
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving a Bitfield message (ID=5).
    ///
    /// Returns [`PeerStateUpdate::SetBitfield`] so the caller can update
    /// piece stats and the peer's bitfield. If the peer is a seeder and
    /// our download is finished, also returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtBitfieldMessage::doReceivedAction()`.
    pub fn on_bitfield_received(
        &mut self,
        bitfield: Vec<u8>,
        is_seeder: bool,
        download_finished: bool,
    ) -> Vec<PeerStateUpdate> {
        trace!(
            "PeerHandler: Bitfield received ({} bytes, seeder={})",
            bitfield.len(),
            is_seeder
        );

        let mut updates = vec![PeerStateUpdate::SetBitfield { data: bitfield }];

        if is_seeder && download_finished {
            debug!("PeerHandler: peer is seeder per Bitfield and download finished — disconnect");
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving a Request message (ID=6) for upload.
    ///
    /// Returns the appropriate [`RequestResponse`]:
    /// - `Piece` if we have the piece and are not choking (or it's in our
    ///   allowed-fast set) — the caller should queue the piece data.
    /// - `Reject` if we are choking and fast extension is enabled.
    /// - `None` if we are choking and fast extension is NOT enabled (drop).
    ///
    /// The `has_piece` closure checks whether we have the requested piece.
    /// The `is_in_am_allowed_fast` closure checks whether the piece index
    /// is in our allowed-fast set (fast extension).
    ///
    /// Mirrors C++ `BtRequestMessage::doReceivedAction()`.
    pub fn on_request_received<F1, F2>(
        &self,
        index: u32,
        begin: u32,
        length: u32,
        has_piece: F1,
        is_in_am_allowed_fast: F2,
    ) -> RequestResponse
    where
        F1: Fn(u32) -> bool,
        F2: Fn(u32) -> bool,
    {
        if has_piece(index) && (!self.am_choking || is_in_am_allowed_fast(index)) {
            trace!(
                "PeerHandler: Request received for piece={} begin={} len={} — will send Piece",
                index, begin, length
            );
            RequestResponse::Piece {
                index,
                begin,
                length,
            }
        } else if self.fast_extension_enabled {
            debug!(
                "PeerHandler: Request rejected (choking, fast ext) for piece={} begin={} len={}",
                index, begin, length
            );
            RequestResponse::Reject {
                index,
                begin,
                length,
            }
        } else {
            debug!(
                "PeerHandler: Request dropped (choking, no fast ext) for piece={} begin={} len={}",
                index, begin, length
            );
            RequestResponse::None
        }
    }

    /// Handle receiving a Cancel message from the peer.
    ///
    /// Invalidates any queued Piece message that matches the specified block.
    /// Mirrors C++ `DefaultBtMessageDispatcher::doCancelSendingPieceAction()`.
    pub fn on_cancel_received(&mut self, index: u32, begin: u32, length: u32) {
        self.dispatcher
            .do_cancel_sending_piece_action(index, begin, length);
        debug!(
            "PeerHandler: cancel received for piece={} begin={} len={}",
            index, begin, length
        );
    }

    /// Handle receiving a KeepAlive message from the peer.
    ///
    /// Increments the flooding stat keepalive counter.
    /// Mirrors C++ `DefaultBtInteractive::receiveMessages()` which calls
    /// `floodingStat_.incKeepAliveCount()` for KeepAlive messages.
    pub fn on_keepalive_received(&mut self) {
        self.flooding_stat.inc_keepalive_count();
        debug!("PeerHandler: keepalive received");
    }

    /// Handle receiving a HaveAll message (ID=14).
    ///
    /// Returns [`PeerStateUpdate::MarkSeeder`] so the caller can mark the peer
    /// as a seeder and update piece stats. If download is finished, also
    /// returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtHaveAllMessage::doReceivedAction()`.
    pub fn on_have_all_received(&mut self, download_finished: bool) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: HaveAll received");

        let mut updates = vec![PeerStateUpdate::MarkSeeder];

        if download_finished {
            debug!("PeerHandler: HaveAll and download finished — disconnect seeder");
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving a HaveNone message (ID=15).
    ///
    /// Returns [`PeerStateUpdate::ClearBitfield`] so the caller can update
    /// piece stats and clear the peer's bitfield.
    ///
    /// Mirrors C++ `BtHaveNoneMessage::doReceivedAction()`.
    pub fn on_have_none_received(&mut self) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: HaveNone received");
        vec![PeerStateUpdate::ClearBitfield]
    }
}
