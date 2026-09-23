use std::time::{Duration, Instant};

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_progress_info_file::{BtProgress, DownloadStats as ProgressDownloadStats};

fn progress_snapshot(
    info_hash: [u8; 20],
    bitfield: &[u8],
    piece_length: u32,
    total_size: u64,
    num_pieces: u32,
    stats: ProgressDownloadStats,
) -> BtProgress {
    let upload_length = stats.uploaded_bytes;
    BtProgress {
        info_hash,
        bitfield: bitfield.to_vec(),
        peers: vec![],
        stats,
        piece_length,
        total_size,
        num_pieces,
        upload_length,
        in_flight_pieces: vec![],
        is_torrent: true,
        save_time: std::time::SystemTime::now(),
        version: 1,
    }
}

fn count_piece_sources(connections: &[BtPeerConn], piece_index: usize) -> usize {
    connections
        .iter()
        .filter(|connection| connection.seeder || connection.has_piece(piece_index))
        .count()
}

fn sync_peer_snapshots(
    group: &crate::request::request_group::RequestGroup,
    active_connections: &[BtPeerConn],
) {
    let snapshots: Vec<crate::request::request_group::BtPeerSnapshot> = active_connections
        .iter()
        .filter_map(BtPeerConn::snapshot)
        .collect();
    group.set_bt_connection_count(snapshots.len());
    group.set_bt_peer_snapshots(snapshots);
}
/// Tracks consecutive BitTorrent time without a completed piece.
///
/// The original `BtStopDownloadCommand` observes the download periodically
/// and resets its checkpoint when the measured download speed is positive.
/// The piece loop already owns the authoritative completed-byte counter, so
/// using it here avoids a stale cached speed keeping a stalled task alive.
struct BtStopTimeoutState {
    configured: Option<Duration>,
    last_progress_at: Instant,
    last_completed_bytes: u64,
}

impl BtStopTimeoutState {
    fn new(now: Instant, completed_bytes: u64) -> Self {
        Self {
            configured: None,
            last_progress_at: now,
            last_completed_bytes: completed_bytes,
        }
    }

    fn should_halt(
        &mut self,
        configured_seconds: Option<u64>,
        completed_bytes: u64,
        now: Instant,
    ) -> bool {
        let configured = configured_seconds
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs);
        if configured != self.configured {
            self.configured = configured;
            self.last_progress_at = now;
        }

        if completed_bytes > self.last_completed_bytes {
            self.last_completed_bytes = completed_bytes;
            self.last_progress_at = now;
        }

        configured.is_some_and(|timeout| now.duration_since(self.last_progress_at) >= timeout)
    }

    fn deadline(&self) -> Option<Instant> {
        self.configured
            .map(|timeout| self.last_progress_at + timeout)
    }
}

mod peer_events;
mod session;

#[cfg(test)]
mod tests;
