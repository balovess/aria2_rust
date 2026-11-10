//! Peer protocol helpers used by the long-lived peer actor.

use crate::engine::bittorrent::peer::connection::BtPeerConn;
use tracing::{debug, trace};

/// Decode ut_pex received while the actor is reading a peer connection.
pub(crate) fn process_pex_during_read(conn: &mut BtPeerConn, ext_id: u8, payload: &[u8]) {
    if !conn.is_pex_enabled() {
        return;
    }

    use aria2_protocol::bittorrent::message::extension::UtPexMessage;
    match UtPexMessage::from_payload(payload) {
        Ok(pex_msg) => {
            conn.pending_pex_peers
                .extend(pex_msg.added.iter().map(|peer| peer.to_peer_addr()));
            conn.pending_pex_peers
                .extend(pex_msg.added6.iter().map(|peer| peer.to_peer_addr()));
            if !pex_msg.added.is_empty() || !pex_msg.added6.is_empty() {
                debug!(
                    "[BT] PEX during actor read: ext_id={}, {} v4 + {} v6 peers (buffered for swarm admission)",
                    ext_id,
                    pex_msg.added.len(),
                    pex_msg.added6.len()
                );
            }
        }
        Err(_) => trace!(
            "[BT] Extended message ext_id={} not recognized as PEX during actor read",
            ext_id
        ),
    }
}
