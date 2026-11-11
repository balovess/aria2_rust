//! Data model for peers discovered through Local Peer Discovery.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::is_private_address;
/// Information about a peer discovered via LPD
#[derive(Debug, Clone)]
pub struct LpdPeer {
    /// The torrent info hash this peer is sharing
    pub info_hash: Arc<str>,
    /// The peer's listen port
    pub port: u16,
    /// The peer's IP address (from recv_from)
    pub addr: IpAddr,
    /// When this peer was last announced
    pub last_seen: Instant,
    /// Whether this peer is on the local network (private IP range).
    /// C++: `peer->setLocalPeer(util::inPrivateAddress(remoteEndpoint.addr))`
    pub is_local: bool,
}

impl LpdPeer {
    /// Create a new LpdPeer
    pub fn new(info_hash: impl Into<Arc<str>>, port: u16, addr: IpAddr) -> Self {
        Self {
            info_hash: info_hash.into(),
            port,
            addr,
            last_seen: Instant::now(),
            is_local: is_private_address(&addr),
        }
    }

    /// Get the peer's address as SocketAddr
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.port)
    }

    /// Check if this peer has expired based on age
    pub fn is_expired(&self, max_age: Duration) -> bool {
        self.last_seen.elapsed() > max_age
    }
}

impl PartialEq for LpdPeer {
    fn eq(&self, other: &Self) -> bool {
        // Two peers are considered equal if they share same info_hash and IP
        self.info_hash == other.info_hash && self.addr == other.addr
    }
}

impl Eq for LpdPeer {}

impl std::hash::Hash for LpdPeer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.info_hash.hash(state);
        self.addr.hash(state);
    }
}
