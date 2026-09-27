//! Request-generation tracking owned by the peer's I/O loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::pipelined::BlockRequest;

static NEXT_REQUEST_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RequestGeneration(u64);

impl RequestGeneration {
    pub(crate) fn allocate() -> Self {
        Self(NEXT_REQUEST_GENERATION.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn is_at_least(self, other: Self) -> bool {
        self.0 >= other.0
    }
}

#[derive(Default)]
pub(super) struct PeerRequestLedger {
    requests: HashMap<(u32, u32), (RequestGeneration, BlockRequest)>,
}

impl PeerRequestLedger {
    pub(super) fn len(&self) -> usize {
        self.requests.len()
    }

    pub(super) fn contains(&self, piece_index: u32, request: BlockRequest) -> bool {
        self.requests.contains_key(&(piece_index, request.offset))
    }

    pub(super) fn record(
        &mut self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) {
        self.requests
            .insert((piece_index, request.offset), (generation, request));
    }

    pub(super) fn cancel(
        &mut self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> bool {
        let key = (piece_index, request.offset);
        if self
            .requests
            .get(&key)
            .is_none_or(|(active, _)| *active != generation)
        {
            return false;
        }
        self.requests.remove(&key);
        true
    }

    pub(super) fn complete(&mut self, piece_index: u32, offset: u32) -> Option<RequestGeneration> {
        self.requests
            .remove(&(piece_index, offset))
            .map(|(generation, _)| generation)
    }

    pub(super) fn drain_generation(
        &mut self,
        generation: RequestGeneration,
    ) -> Vec<(u32, BlockRequest)> {
        let stale = self
            .requests
            .iter()
            .filter(|(_, (active, _))| *active != generation)
            .map(|(key, (_, request))| (key.0, *request))
            .collect::<Vec<_>>();
        for (piece_index, request) in &stale {
            self.requests.remove(&(*piece_index, request.offset));
        }
        stale
    }

    pub(super) fn drain_exact_generation(
        &mut self,
        generation: RequestGeneration,
    ) -> Vec<(u32, BlockRequest)> {
        let matching = self
            .requests
            .iter()
            .filter(|(_, (active, _))| *active == generation)
            .map(|(key, (_, request))| (key.0, *request))
            .collect::<Vec<_>>();
        for (piece_index, request) in &matching {
            self.requests.remove(&(*piece_index, request.offset));
        }
        matching
    }

    pub(super) fn drain_all(&mut self) -> Vec<(u32, BlockRequest)> {
        self.requests
            .drain()
            .map(|((piece_index, _), (_, request))| (piece_index, request))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::pipelined::BlockRequest;
    use super::{PeerRequestLedger, RequestGeneration};

    fn request() -> BlockRequest {
        BlockRequest {
            block_index: 2,
            offset: 32 * 1024,
            length: 16 * 1024,
        }
    }

    #[test]
    fn old_generation_cancel_does_not_remove_current_request() {
        let old_generation = RequestGeneration(10);
        let current_generation = RequestGeneration(11);
        let mut ledger = PeerRequestLedger::default();
        ledger.record(current_generation, 4, request());
        assert_eq!(ledger.len(), 1);

        assert!(!ledger.cancel(old_generation, 4, request()));
        assert_eq!(
            ledger.complete(4, request().offset),
            Some(current_generation)
        );
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn generation_change_drains_prior_inflight_requests() {
        let old_generation = RequestGeneration(20);
        let current_generation = RequestGeneration(21);
        let request = request();
        let mut ledger = PeerRequestLedger::default();
        ledger.record(old_generation, 7, request);

        assert_eq!(
            ledger.drain_generation(current_generation),
            vec![(7, request)]
        );
        assert_eq!(ledger.complete(7, request.offset), None);
    }

    #[test]
    fn shutdown_drain_returns_requests_from_every_generation() {
        let mut ledger = PeerRequestLedger::default();
        let first = request();
        let second = BlockRequest {
            block_index: 3,
            offset: 48 * 1024,
            length: 16 * 1024,
        };
        ledger.record(RequestGeneration(30), 7, first);
        ledger.record(RequestGeneration(31), 8, second);

        let drained = ledger.drain_all();

        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&(7, first)));
        assert!(drained.contains(&(8, second)));
        assert!(ledger.drain_all().is_empty());
    }
}
