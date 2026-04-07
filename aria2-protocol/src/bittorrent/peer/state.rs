use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::bittorrent::message::types::PieceBlockRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerChokingState {
    Choked,
    Unchoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerInterestState {
    Interested,
    NotInterested,
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
    pub outgoing_requests: HashSet<PieceBlockRequest>,
    pub download_speed: f64,
    pub upload_speed: f64,
    pub last_message_time: Instant,
    pub connection_established: Instant,
    pub bytes_downloaded: u64,
    pub bytes_uploaded: u64,
}

impl Default for PeerState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            outgoing_requests: HashSet::new(),
            download_speed: 0.0,
            upload_speed: 0.0,
            last_message_time: now,
            connection_established: now,
            bytes_downloaded: 0,
            bytes_uploaded: 0,
        }
    }
}

impl PeerState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn can_download_from(&self) -> bool {
        !self.peer_choking && self.am_interested
    }

    pub fn can_upload_to(&self) -> bool {
        !self.am_choking && self.peer_interested
    }

    pub fn is_active(&self) -> bool {
        self.am_interested || self.peer_interested
    }

    pub fn add_request(&mut self, req: PieceBlockRequest) -> bool {
        self.outgoing_requests.insert(req)
    }

    pub fn remove_request(&mut self, req: &PieceBlockRequest) -> bool {
        self.outgoing_requests.remove(req)
    }

    pub fn clear_requests(&mut self) {
        self.outgoing_requests.clear();
    }

    pub fn pending_request_count(&self) -> usize {
        self.outgoing_requests.len()
    }

    pub fn update_download_speed(&mut self, bytes: u64, elapsed_secs: f64) {
        self.bytes_downloaded += bytes;
        if elapsed_secs > 0.0 {
            self.download_speed = bytes as f64 / elapsed_secs;
        }
    }

    pub fn update_upload_speed(&mut self, bytes: u64, elapsed_secs: f64) {
        self.bytes_uploaded += bytes;
        if elapsed_secs > 0.0 {
            self.upload_speed = bytes as f64 / elapsed_secs;
        }
    }

    pub fn time_since_last_message(&self) -> Duration {
        self.last_message_time.elapsed()
    }
}

pub struct ChokeAlgorithm;

impl ChokeAlgorithm {
    #[allow(dead_code)]
    const OPTIMISTIC_UNCHOKE_INTERVAL_SECS: u64 = 30;
    #[allow(dead_code)]
    const CHOKE_INTERVAL_SECS: u64 = 10;
    const MAX_UNCHOKED_LEECHERS: usize = 4;
    #[allow(dead_code)]
    const SPEED_CALC_WINDOW_SECS: f64 = 20.0;

    pub fn evaluate_choke(peers: &mut [&mut PeerState], is_seeder: bool) -> Vec<usize> {
        let mut unchoke_indices: Vec<usize> = (0..peers.len()).collect();

        unchoke_indices.sort_by(|&a, &b| {
            peers[b].download_speed.partial_cmp(&peers[a].download_speed).unwrap_or(std::cmp::Ordering::Equal)
        });

        let max_unchoke = if is_seeder {
            peers.len().min(Self::MAX_UNCHOKED_LEECHERS * 3)
        } else {
            Self::MAX_UNCHOKED_LEECHERS
        };

        let optimistic_slot = if !is_seeder && max_unchoke >= 2 {
            Some(max_unchoke - 1)
        } else {
            None
        };

        let regular_count = match optimistic_slot {
            Some(_) => max_unchoke.saturating_sub(1),
            None => max_unchoke,
        };

        let mut to_unchoke = Vec::new();

        for (rank, &idx) in unchoke_indices.iter().enumerate() {
            if rank < regular_count {
                to_unchoke.push(idx);
                peers[idx].am_choking = false;
            } else if let Some(opt_idx) = optimistic_slot {
                if rank == opt_idx {
                    to_unchoke.push(idx);
                    peers[idx].am_choking = false;
                } else {
                    peers[idx].am_choking = true;
                    peers[idx].clear_requests();
                }
            } else {
                peers[idx].am_choking = true;
                peers[idx].clear_requests();
            }
        }

        to_unchoke
    }

    pub fn select_optimistic_unchoke(peers: &[&PeerState], _current_optimistic: Option<usize>) -> Option<usize> {
        let choked_interested: Vec<usize> = peers.iter()
            .enumerate()
            .filter(|(_, p)| p.am_choking && p.peer_interested)
            .map(|(i, _)| i)
            .collect();

        if choked_interested.is_empty() {
            return None;
        }

        use rand::Rng;
        let mut rng = rand::thread_rng();
        let candidate = rng.gen_range(0..choked_interested.len());
        Some(choked_interested[candidate])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_state_defaults() {
        let state = PeerState::new();
        assert!(state.am_choking);
        assert!(state.peer_choking);
        assert!(!state.am_interested);
        assert!(!state.peer_interested);
        assert!(!state.can_download_from());
        assert!(!state.can_upload_to());
    }

    #[test]
    fn test_can_download_when_unchoke_and_interested() {
        let mut state = PeerState::new();
        state.peer_choking = false;
        state.am_interested = true;
        assert!(state.can_download_from());

        state.peer_choking = true;
        assert!(!state.can_download_from());
    }

    #[test]
    fn test_request_management() {
        let mut state = PeerState::new();
        let req1 = PieceBlockRequest::new(0, 0, 16384);
        let req2 = PieceBlockRequest::new(0, 16384, 16384);

        assert!(state.add_request(req1.clone()));
        assert!(!state.add_request(req1.clone()));
        assert!(state.add_request(req2.clone()));
        assert_eq!(state.pending_request_count(), 2);

        assert!(state.remove_request(&req1));
        assert_eq!(state.pending_request_count(), 1);

        state.clear_requests();
        assert_eq!(state.pending_request_count(), 0);
    }

    #[test]
    fn test_choke_algorithm_basic() {
        let mut peers: Vec<PeerState> = vec![
            PeerState { download_speed: 100.0, ..PeerState::new() },
            PeerState { download_speed: 500.0, ..PeerState::new() },
            PeerState { download_speed: 300.0, ..PeerState::new() },
            PeerState { download_speed: 50.0, ..PeerState::new() },
            PeerState { download_speed: 200.0, ..PeerState::new() },
        ];
        let mut refs: Vec<&mut PeerState> = peers.iter_mut().collect();
        let unchoked = ChokeAlgorithm::evaluate_choke(&mut refs, false);

        assert_eq!(unchoked.len(), 4);
        assert!(!refs[1].am_choking);
        assert!(!refs[2].am_choking);
    }
}
