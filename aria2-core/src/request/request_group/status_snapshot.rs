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
    /// Number of pieces marked complete in the current local bitfield.
    pub completed_pieces: u32,
    /// Number of pieces still missing from the current local bitfield.
    pub missing_pieces: u32,
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

    /// Whether the local bitfield is complete for all advertised pieces.
    pub fn is_complete(&self) -> bool {
        self.num_pieces > 0 && self.missing_pieces == 0
    }
}

impl super::RequestGroup {
    /// Capture all protocol-independent progress and BT status fields.
    pub fn status_snapshot(&self) -> DownloadStatusSnapshot {
        let bt_info_hash = self.get_bt_info_hash_hex();
        let bt_num_pieces = self.get_bt_num_pieces();
        let bt = (bt_info_hash.is_some() || bt_num_pieces > 0).then(|| {
            let bitfield = self.get_bt_bitfield();
            let completed_pieces = bitfield
                .as_deref()
                .map(|bits| count_set_pieces(bits, bt_num_pieces))
                .unwrap_or(0);
            BtStatusSnapshot {
                info_hash: bt_info_hash.unwrap_or_default(),
                num_pieces: bt_num_pieces,
                piece_length: self.get_bt_piece_length(),
                missing_pieces: bt_num_pieces.saturating_sub(completed_pieces),
                completed_pieces,
                bitfield,
                peers: self.bt_peer_snapshots(),
            }
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

fn count_set_pieces(bitfield: &[u8], num_pieces: u32) -> u32 {
    (0..num_pieces as usize)
        .filter(|index| {
            let byte = *index / 8;
            let bit = 7 - (*index % 8);
            bitfield
                .get(byte)
                .is_some_and(|value| value & (1 << bit) != 0)
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::count_set_pieces;

    #[test]
    fn count_set_pieces_ignores_trailing_padding_bits() {
        assert_eq!(count_set_pieces(&[0b1111_1111], 5), 5);
    }

    #[test]
    fn count_set_pieces_handles_short_bitfields() {
        assert_eq!(count_set_pieces(&[0b1010_0000], 10), 2);
    }
}
