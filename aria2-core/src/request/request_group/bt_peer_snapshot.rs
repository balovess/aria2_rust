//! Copyable BitTorrent peer data exposed by a request group.

use std::net::SocketAddr;

/// The mechanism that first supplied an address for a connected peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BtPeerSource {
    #[default]
    Unknown,
    Tracker,
    Dht,
    Pex,
    Lpd,
    Incoming,
}

impl BtPeerSource {
    /// Stable lower-case wire value used by RPC consumers.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Tracker => "tracker",
            Self::Dht => "dht",
            Self::Pex => "pex",
            Self::Lpd => "lpd",
            Self::Incoming => "incoming",
        }
    }
}

/// A point-in-time view of one active BitTorrent peer.
#[derive(Debug, Clone, PartialEq)]
pub struct BtPeerSnapshot {
    pub peer_id: [u8; 20],
    pub addr: SocketAddr,
    /// Whether this peer accepted our inbound connection.
    ///
    /// aria2's RPC compatibility rule reports port `0` for incoming peers;
    /// the socket source port is only an ephemeral transport detail.
    pub is_incoming: bool,
    /// Discovery mechanism that supplied this peer address.
    pub source: BtPeerSource,
    /// The peer's raw piece availability bitfield, when received.
    pub bitfield: Option<Vec<u8>>,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub upload_speed: f64,
    pub download_speed: f64,
    pub avg_upload_speed: u64,
    pub avg_download_speed: u64,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub seeder: Option<bool>,
    pub connection_duration_secs: u64,
    pub last_data_age_secs: u64,
    pub is_snubbed: bool,
    pub is_banned: bool,
}
