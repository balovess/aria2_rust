//! Choke/unchoke and interested state management.

use crate::engine::bt_message_dispatcher::RequestSlot;
use tracing::{debug, trace};

use super::types::PeerStateUpdate;
use super::BtPeerMessageHandler;

impl BtPeerMessageHandler {
    // ── Event-Driven Actions ─────────────────────────────────────────────

    /// Handle receiving a Choke message from the peer.
    ///
    /// Removes outstanding request slots for pieces NOT in the allowed-fast set.
    /// Returns removed slots so the caller can send Cancel messages if needed.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokedAction()`.
    pub fn on_choke_received<F>(&mut self, is_in_allowed_fast: F) -> Vec<RequestSlot>
    where
        F: Fn(u32) -> bool,
    {
        self.peer_choking = true;
        self.flooding_stat.inc_choke_unchoke_count();

        let removed = self.dispatcher.do_choked_action(is_in_allowed_fast);
        if !removed.is_empty() {
            debug!(
                "PeerHandler: choke received, removed {} outstanding requests",
                removed.len()
            );
        }
        removed
    }

    /// Handle receiving an Unchoke message from the peer.
    ///
    /// Updates the choking state and increments the flooding stat counter.
    /// Mirrors C++ `DefaultBtInteractive::receiveMessages()` which calls
    /// `floodingStat_.incChokeUnchokeCount()` on state transitions.
    pub fn on_unchoke_received(&mut self) {
        self.peer_choking = false;
        self.flooding_stat.inc_choke_unchoke_count();
        debug!("PeerHandler: unchoke received");
    }

    /// Handle sending a Choke message to the peer.
    ///
    /// Invalidates all queued Piece upload messages since we are choking
    /// the peer and should not send them data.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokingAction()`.
    pub fn on_choke_sent(&mut self) {
        self.dispatcher.do_choking_action();
        debug!("PeerHandler: choke sent, invalidated upload messages");
    }

    // ── Interested / NotInterested Handlers ──────────────────────────────

    /// Handle receiving an Interested message (ID=2).
    ///
    /// Sets `peer_interested = true`. If we are choking the peer, triggers
    /// [`PeerStateUpdate::ExecuteChoke`] so the caller can re-evaluate the
    /// choking algorithm (unchoke if appropriate).
    ///
    /// Mirrors C++ `BtInterestedMessage::doReceivedAction()`.
    pub fn on_interested_received(&mut self) -> Vec<PeerStateUpdate> {
        self.peer_interested = true;
        trace!("PeerHandler: Interested received (peer_interested=true)");

        if self.am_choking {
            debug!("PeerHandler: Interested while am_choking — trigger executeChoke");
            vec![PeerStateUpdate::ExecuteChoke]
        } else {
            vec![]
        }
    }

    /// Handle receiving a NotInterested message (ID=3).
    ///
    /// Sets `peer_interested = false`. If we are NOT choking the peer,
    /// triggers [`PeerStateUpdate::ExecuteChoke`] so the caller can
    /// re-evaluate the choking algorithm (may choke this peer to free
    /// an upload slot).
    ///
    /// Mirrors C++ `BtNotInterestedMessage::doReceivedAction()`.
    pub fn on_not_interested_received(&mut self) -> Vec<PeerStateUpdate> {
        self.peer_interested = false;
        trace!("PeerHandler: NotInterested received (peer_interested=false)");

        if !self.am_choking {
            debug!("PeerHandler: NotInterested while not am_choking — trigger executeChoke");
            vec![PeerStateUpdate::ExecuteChoke]
        } else {
            vec![]
        }
    }

    // ── Choking State Accessors ─────────────────────────────────────────

    /// Set whether we are choking the peer (amChoking).
    ///
    /// Mirrors C++ `peer->amChoking(true/false)`.
    pub fn set_am_choking(&mut self, choking: bool) {
        self.am_choking = choking;
    }

    /// Check if we are choking the peer.
    pub fn is_am_choking(&self) -> bool {
        self.am_choking
    }

    /// Check if the peer is interested in our data.
    pub fn is_peer_interested(&self) -> bool {
        self.peer_interested
    }

    /// Return whether this peer is currently choking us.
    pub fn is_peer_choking(&self) -> bool {
        self.peer_choking
    }

    /// Return whether this peer has been marked as snubbing.
    pub fn is_peer_snubbing(&self) -> bool {
        self.peer_snubbing
    }
}
