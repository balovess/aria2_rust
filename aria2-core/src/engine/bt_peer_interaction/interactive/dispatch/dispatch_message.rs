//! Message dispatch for `BtPeerInteractive`.
//!
//! Contains the central message dispatch that routes each received
//! `BtMessage` variant to the appropriate handler method.

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::extension_registry;
use crate::engine::extension_registry::ExtensionUpdate;
use aria2_protocol::bittorrent::message::types::BtMessage;
use tracing::{debug, trace, warn};

use super::super::BtPeerInteractive;
use crate::engine::bt_peer_interaction::types::*;

impl BtPeerInteractive {
    // -- Message dispatch -----------------------------------------------------

    /// Dispatch a received message to the appropriate handler method.
    ///
    /// This is the central message dispatch that the C++ code handles
    /// via virtual dispatch on `BtMessage::doReceivedAction()`. Each
    /// message type is routed to the corresponding `on_*_received()`
    /// method on the handler, and internal state (peer_choking,
    /// peer_interested, flooding stats) is updated.
    ///
    /// # Arguments
    /// * `msg` - The received BtMessage to dispatch
    /// * `conn` - The peer connection (for AllowedFast set access)
    /// * `is_in_allowed_fast` - Closure checking if a piece is in the
    ///   peer's allowed-fast set (needed for Choke handling)
    ///
    /// # Returns
    ///
    /// A [DispatchUpdate] containing state changes for the caller to apply.
    pub(crate) fn dispatch_message<F>(
        &mut self,
        msg: BtMessage,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> DispatchUpdate
    where
        F: Fn(u32) -> bool,
    {
        let mut update = DispatchUpdate::default();

        match msg {
            BtMessage::Choke => {
                let was_choking = self.peer_choking;
                // Delegate to handler: removes non-allowed-fast request slots
                update.cancelled_slots = self.handler.on_choke_received(is_in_allowed_fast);
                self.peer_choking = true;
                update.peer_choking_changed = !was_choking;
                update.peer_choking = true;
                // Update flooding stat for transition detection
                if !was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Choke message");
            }
            BtMessage::Unchoke => {
                let was_choking = self.peer_choking;
                self.handler.on_unchoke_received();
                self.peer_choking = false;
                update.peer_choking_changed = was_choking;
                update.peer_choking = false;
                // Update flooding stat for transition detection
                if was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Unchoke message");
            }
            BtMessage::Interested => {
                self.peer_interested = true;
                trace!("Dispatched Interested message");
            }
            BtMessage::NotInterested => {
                self.peer_interested = false;
                trace!("Dispatched NotInterested message");
            }
            BtMessage::Have { piece_index } => {
                // Update the peer's bitfield and expose the exact transition.
                if let Some(ref mut res) = conn.session_resource {
                    let old = res.bitfield().to_vec();
                    res.update_bitfield(piece_index as usize, 1);
                    let new = res.bitfield().to_vec();
                    update.bitfield_update = Some(BitfieldUpdate { old, new });
                }
                if let Some(ref res) = conn.session_resource
                    && res.is_seeder()
                {
                    conn.seeder = true;
                }
                update.have_index = Some(piece_index);
                trace!("Dispatched Have({}) message", piece_index);
            }
            BtMessage::Bitfield { data } => {
                // Update the peer's bitfield from the full bitfield message
                if let Some(ref mut res) = conn.session_resource {
                    let old = res.set_bitfield(&data);
                    let new = res.bitfield().to_vec();
                    update.bitfield_update = Some(BitfieldUpdate { old, new });
                    if res.is_seeder() {
                        conn.seeder = true;
                    }
                }
                update.bitfield_data = Some(data);
                trace!("Dispatched Bitfield message");
            }
            BtMessage::Request { request } => {
                // Incoming request from peer to upload data.
                // Record data exchange for active interaction checking.
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Request(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::Piece {
                index,
                begin,
                ref data,
            } => {
                // Received piece data - remove matching request slot
                self.handler
                    .on_piece_received(index, begin, data.len() as u32);
                // Record data exchange for active interaction checking
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Piece(index={}, begin={}, len={})",
                    index,
                    begin,
                    data.len()
                );
            }
            BtMessage::Cancel { request } => {
                // Peer cancels a pending upload
                self.handler
                    .on_cancel_received(request.index, request.begin, request.length);
                trace!(
                    "Dispatched Cancel(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::KeepAlive => {
                self.handler.on_keepalive_received();
                self.flooding_stat.inc_keepalive_count();
                trace!("Dispatched KeepAlive message");
            }
            BtMessage::Port { port } => {
                // DHT port message (BEP 5)
                if self.dht_enabled {
                    if let Some(handler) = &self.dht_port_handler {
                        handler(port);
                    }
                    trace!("Dispatched Port({}) message", port);
                }
            }
            BtMessage::AllowedFast { index } => {
                // BEP 6: peer grants fast access to a piece
                conn.add_allowed_fast(index);
                trace!("Dispatched AllowedFast({}) message", index);
            }
            BtMessage::Reject {
                index,
                offset,
                length,
            } => {
                // BEP 6: peer rejected our request.
                // Remove the matching outstanding request slot - do NOT treat
                // this like a Piece message (the data was NOT received).
                // Mirrors C++ `BtRejectMessage::doReceivedAction()`.
                if let Err(e) = self.handler.on_reject_received(index, offset, length) {
                    debug!("Reject handler error: {}", e);
                }
                trace!(
                    "Dispatched Reject(piece={}, offset={}, len={})",
                    index, offset, length
                );
            }
            BtMessage::Suggest { index } => {
                // BEP 6: peer suggests we download this piece
                // The caller should boost the priority of this piece
                trace!("Dispatched Suggest({}) message", index);
            }
            BtMessage::HaveAll => {
                // BEP 6: peer has all pieces
                if let Some(ref mut res) = conn.session_resource {
                    let old = res.bitfield().to_vec();
                    res.mark_seeder();
                    let new = res.bitfield().to_vec();
                    update.bitfield_update = Some(BitfieldUpdate { old, new });
                }
                conn.seeder = true;
                trace!("Dispatched HaveAll message");
            }
            BtMessage::HaveNone => {
                // BEP 6: peer has no pieces.
                // Clear the peer's bitfield to reflect this.
                // Mirrors C++ `BtHaveNoneMessage::doReceivedAction()`.
                if let Some(ref mut res) = conn.session_resource {
                    let old = res.bitfield().to_vec();
                    res.clear_bitfield();
                    let new = res.bitfield().to_vec();
                    update.bitfield_update = Some(BitfieldUpdate { old, new });
                }
                conn.seeder = false;
                trace!("Dispatched HaveNone message");
            }
            BtMessage::Extended {
                ext_id,
                ref payload,
            } => {
                // BEP 10: extension protocol message.
                // Dispatch via the extension registry which handles:
                //   ext_id == 0 -> Extension Handshake (BEP 10)
                //   ext_id == peer_ut_metadata_id -> ut_metadata (BEP 9)
                //   ext_id == peer_ut_pex_id -> ut_pex (BEP 11)
                //   otherwise -> unknown extension
                let ext_update = extension_registry::dispatch_extension_message(
                    &mut self.extension_registry,
                    ext_id,
                    payload,
                );

                if let Some(ref update) = ext_update {
                    match update {
                        ExtensionUpdate::HandshakeReceived { .. } => {
                            // Keep the per-connection registry synchronized with the
                            // negotiated IDs used by outbound extension messages.
                            if let Some(id) = self.extension_registry.peer_ut_pex_id() {
                                conn.register_peer_extension("ut_pex", id);
                            }
                            if let Some(id) = self.extension_registry.peer_ut_metadata_id() {
                                conn.register_peer_extension("ut_metadata", id);
                            }

                            // Enable PEX only when the peer advertised it.
                            if self.extension_registry.supports_ut_pex() {
                                self.ut_pex_enabled = true;
                                debug!("ut_pex enabled after extension handshake");
                            }
                            debug!(
                                "Dispatched Extended handshake (ut_metadata={:?}, ut_pex={:?})",
                                self.extension_registry.peer_ut_metadata_id(),
                                self.extension_registry.peer_ut_pex_id()
                            );
                        }
                        ExtensionUpdate::MetadataPiece { piece, .. } => {
                            debug!("Dispatched Extended ut_metadata Data(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataRequest { piece } => {
                            debug!("Dispatched Extended ut_metadata Request(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataReject { piece } => {
                            debug!("Dispatched Extended ut_metadata Reject(piece={})", piece);
                        }
                        ExtensionUpdate::PeerExchange {
                            added_v4,
                            added_v6,
                            dropped_v4,
                            dropped_v6,
                        } => {
                            debug!(
                                "Dispatched Extended ut_pex ({} v4 added, {} v6 added, {} v4 dropped, {} v6 dropped)",
                                added_v4.len(),
                                added_v6.len(),
                                dropped_v4.len(),
                                dropped_v6.len()
                            );
                        }
                    }
                } else {
                    warn!(
                        "Dispatched Extended with unknown ext_id={} (payload_len={})",
                        ext_id,
                        payload.len()
                    );
                }

                if let Some(ref ext_update) = ext_update
                    && let Some(handler) = &self.extension_update_handler
                {
                    handler(ext_update);
                }
                update.extension_update = ext_update;
            }
        }

        update
    }
}
