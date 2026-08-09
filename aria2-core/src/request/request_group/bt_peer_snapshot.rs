//! Copyable BitTorrent peer data exposed by a request group.

use std::net::SocketAddr;

/// A point-in-time view of one active BitTorrent peer.
#[derive(Debug, Clone, PartialEq)]
pub struct BtPeerSnapshot {
    pub peer_id: [u8; 20],
    pub addr: SocketAddr,
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
