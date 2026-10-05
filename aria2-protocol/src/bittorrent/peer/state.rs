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
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    outgoing_requests: HashSet<PieceBlockRequest>,
    download_speed: f64,
    upload_speed: f64,
    last_message_time: Instant,
    connection_established: Instant,
    bytes_downloaded: u64,
    bytes_uploaded: u64,
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

    pub fn am_choking(&self) -> bool {
        self.am_choking
    }

    pub fn am_interested(&self) -> bool {
        self.am_interested
    }

    pub fn peer_choking(&self) -> bool {
        self.peer_choking
    }

    pub fn peer_interested(&self) -> bool {
        self.peer_interested
    }

    pub fn outgoing_requests(&self) -> &HashSet<PieceBlockRequest> {
        &self.outgoing_requests
    }

    pub fn download_speed(&self) -> f64 {
        self.download_speed
    }

    pub fn upload_speed(&self) -> f64 {
        self.upload_speed
    }

    pub fn connection_established(&self) -> Instant {
        self.connection_established
    }

    pub fn last_message_time(&self) -> Instant {
        self.last_message_time
    }

    pub fn bytes_downloaded(&self) -> u64 {
        self.bytes_downloaded
    }

    pub fn bytes_uploaded(&self) -> u64 {
        self.bytes_uploaded
    }

    pub fn set_peer_choking(&mut self, choking: bool) {
        self.peer_choking = choking;
    }

    pub fn set_peer_interested(&mut self, interested: bool) {
        self.peer_interested = interested;
    }

    pub fn set_am_choking(&mut self, choking: bool) {
        self.am_choking = choking;
    }

    pub fn set_am_interested(&mut self, interested: bool) {
        self.am_interested = interested;
    }

    pub fn mark_message_received(&mut self) {
        self.last_message_time = Instant::now();
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
    const MAX_UNCHOKED_LEECHERS: usize = 4;

    pub fn evaluate_choke(peers: &mut [&mut PeerState], is_seeder: bool) -> Vec<usize> {
        let mut unchoke_indices: Vec<usize> = (0..peers.len()).collect();

        unchoke_indices.sort_by(|&a, &b| {
            peers[b]
                .download_speed()
                .partial_cmp(&peers[a].download_speed())
                .unwrap_or(std::cmp::Ordering::Equal)
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
                peers[idx].set_am_choking(false);
            } else if let Some(opt_idx) = optimistic_slot {
                if rank == opt_idx {
                    to_unchoke.push(idx);
                    peers[idx].set_am_choking(false);
                } else {
                    peers[idx].set_am_choking(true);
                    peers[idx].clear_requests();
                }
            } else {
                peers[idx].set_am_choking(true);
                peers[idx].clear_requests();
            }
        }

        to_unchoke
    }

    pub fn select_optimistic_unchoke(
        peers: &[&PeerState],
        _current_optimistic: Option<usize>,
    ) -> Option<usize> {
        let choked_interested: Vec<usize> = peers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.am_choking() && p.peer_interested())
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
        assert!(state.am_choking());
        assert!(state.peer_choking());
        assert!(!state.am_interested());
        assert!(!state.peer_interested());
        assert!(!state.can_download_from());
        assert!(!state.can_upload_to());
    }

    #[test]
    fn test_can_download_when_unchoke_and_interested() {
        let mut state = PeerState::new();
        state.set_peer_choking(false);
        state.set_am_interested(true);
        assert!(state.can_download_from());

        state.set_peer_choking(true);
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
    fn test_peer_state_accessors_and_counters() {
        let mut state = PeerState::new();
        let initial_message_time = state.last_message_time();

        state.set_peer_interested(true);
        state.update_download_speed(120, 2.0);
        state.update_upload_speed(60, 3.0);
        state.mark_message_received();

        assert!(state.peer_interested());
        assert_eq!(state.download_speed(), 60.0);
        assert_eq!(state.upload_speed(), 20.0);
        assert_eq!(state.bytes_downloaded(), 120);
        assert_eq!(state.bytes_uploaded(), 60);
        assert!(state.outgoing_requests().is_empty());
        assert!(state.last_message_time() >= initial_message_time);
    }

    #[test]
    fn test_choke_algorithm_basic() {
        let mut peers: Vec<PeerState> = vec![
            {
                let mut state = PeerState::new();
                state.update_download_speed(100, 1.0);
                state
            },
            {
                let mut state = PeerState::new();
                state.update_download_speed(500, 1.0);
                state
            },
            {
                let mut state = PeerState::new();
                state.update_download_speed(300, 1.0);
                state
            },
            {
                let mut state = PeerState::new();
                state.update_download_speed(50, 1.0);
                state
            },
            {
                let mut state = PeerState::new();
                state.update_download_speed(200, 1.0);
                state
            },
        ];
        let mut refs: Vec<&mut PeerState> = peers.iter_mut().collect();
        let unchoked = ChokeAlgorithm::evaluate_choke(&mut refs, false);

        assert_eq!(unchoked.len(), 4);
        assert!(!refs[1].am_choking());
        assert!(!refs[2].am_choking());
    }
}
