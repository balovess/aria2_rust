//! Peer protocol helpers used by the long-lived peer actor.

use crate::engine::bt_peer_connection::BtPeerConn;
use tracing::{debug, trace};

/// Decode ut_pex received while the actor is reading a peer connection.
pub(crate) fn process_pex_during_read(conn: &mut BtPeerConn, ext_id: u8, payload: &[u8]) {
    if !conn.is_pex_enabled() {
        return;
    }

    use aria2_protocol::bittorrent::message::extension::UtPexMessage;
    use aria2_protocol::bittorrent::peer::connection::PeerAddr;

    match UtPexMessage::from_payload(payload) {
        Ok(pex_msg) => {
            for compact in &pex_msg.added {
                let ip = std::net::Ipv4Addr::from(*compact.ip());
                conn.pending_pex_peers
                    .push(PeerAddr::new(&ip.to_string(), compact.port()));
            }
            for compact in &pex_msg.added6 {
                let ip = std::net::Ipv6Addr::from(*compact.ip());
                conn.pending_pex_peers
                    .push(PeerAddr::new(&ip.to_string(), compact.port()));
            }
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
