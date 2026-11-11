//! LPD message compatibility entrypoint and local-address policy.

use std::net::IpAddr;

use tracing::debug;

use super::LpdPeer;

/// Parse a local peer discovery announcement and attach its observed sender.
pub fn parse_lpd_announcement(data: &[u8], sender_ip: IpAddr) -> Option<LpdPeer> {
    let announcement = aria2_protocol::bittorrent::lpd::parse_announcement(data)?;
    debug!(
        info_hash = %&announcement.info_hash[..8],
        port = announcement.port,
        addr = %sender_ip,
        "Received valid BEP14 LPD announcement"
    );
    Some(LpdPeer::new(
        announcement.info_hash,
        announcement.port,
        sender_ip,
    ))
}

/// Check whether an address belongs to the private/reserved ranges used by
/// aria2's local-peer classification policy.
pub fn is_private_address(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 10
                || (octets[0] == 172 && (octets[1] & 0xf0) == 16)
                || (octets[0] == 192 && octets[1] == 168)
                || octets[0] == 127
                || (octets[0] == 169 && octets[1] == 254)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            (segments[0] & 0xfe00) == 0xfc00 || v6.is_loopback()
        }
    }
}
