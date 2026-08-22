//! Protocol-neutral status snapshots shared by CLI, TUI, and RPC adapters.

use std::time::Duration;

use super::{BtPeerSnapshot, DownloadStatus};

/// A point-in-time view of one request group for read-only consumers.
///
/// The snapshot owns all values so callers do not hold the request-group lock
/// while formatting output or serializing an RPC response.
#[derive(Debug, Clone)]
pub struct DownloadStatusSnapshot {
    pub status: DownloadStatus,
    pub total_length: u64,
    pub completed_length: u64,
    pub upload_length: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    /// Number of currently active protocol connections.
    pub connections: u32,
    pub elapsed: Option<Duration>,
    pub bt: Option<BtStatusSnapshot>,
}

/// BitTorrent-specific portion of [`DownloadStatusSnapshot`].
#[derive(Debug, Clone)]
pub struct BtStatusSnapshot {
    pub info_hash: String,
    pub num_pieces: u32,
    pub piece_length: u32,
    pub bitfield: Option<Vec<u8>>,
    pub peers: Vec<BtPeerSnapshot>,
}

impl BtStatusSnapshot {
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn seeder_count(&self) -> usize {
        self.peers
            .iter()
            .filter(|peer| peer.seeder == Some(true))
            .count()
    }
}

impl super::RequestGroup {
    /// Capture all protocol-independent progress and BT status fields.
    pub fn status_snapshot(&self) -> DownloadStatusSnapshot {
        let bt_info_hash = self.get_bt_info_hash_hex();
        let bt_num_pieces = self.get_bt_num_pieces();
        let bt = (bt_info_hash.is_some() || bt_num_pieces > 0).then(|| BtStatusSnapshot {
            info_hash: bt_info_hash.unwrap_or_default(),
            num_pieces: bt_num_pieces,
            piece_length: self.get_bt_piece_length(),
            bitfield: self.get_bt_bitfield(),
            peers: self.bt_peer_snapshots(),
        });

        DownloadStatusSnapshot {
            status: self.status(),
            total_length: self.get_total_length_atomic(),
            completed_length: self.get_completed_length(),
            upload_length: self.get_uploaded_length(),
            download_speed: self.get_download_speed_cached(),
            upload_speed: self.get_upload_speed_cached(),
            connections: self.active_connection_count(),
            elapsed: self.elapsed_time(),
            bt,
        }
    }
}
