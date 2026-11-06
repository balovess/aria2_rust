use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use dashmap::DashMap;
use tokio::sync::OnceCell;

use crate::error::Aria2Error;

use super::MAX_SERVER_CONCURRENCY;
pub(super) struct AuthorityState {
    /// Per-download logical Range budget (`split`).
    pub(super) hard_limit: usize,
    /// Per-download physical connection ceiling (`max-connection-per-server`).
    pub(super) connection_limit: usize,
    pub(super) target: AtomicUsize,
    pub(super) in_flight: AtomicUsize,
    pub(super) client_in_flight: Arc<Vec<AtomicUsize>>,
    pub(super) http1_client_slot_limit: usize,
    pub(super) next_client_index: AtomicUsize,
    pub(super) protocol: AtomicU8,
    pub(super) active_h2_sessions: AtomicUsize,
    pub(super) client_warmups: DashMap<usize, Arc<OnceCell<std::result::Result<(), Aria2Error>>>>,
}

pub(super) struct ExecutorState {
    pub(super) authorities: DashMap<Box<str>, Arc<AuthorityState>>,
    pub(super) total_in_flight: AtomicUsize,
}

pub(super) struct AdmissionLease {
    state: Arc<ExecutorState>,
    authority: Arc<AuthorityState>,
    client_in_flight: Arc<Vec<AtomicUsize>>,
    client_index: usize,
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.authority.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.state.total_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.client_in_flight[self.client_index].fetch_sub(1, Ordering::AcqRel);
    }
}

impl AdmissionLease {
    pub(super) fn try_reassign_client_slot(&mut self, client_index: usize, limit: usize) -> bool {
        if self.client_index == client_index {
            return true;
        }
        if !reserve_bounded(&self.client_in_flight[client_index], limit) {
            return false;
        }
        self.client_in_flight[self.client_index].fetch_sub(1, Ordering::AcqRel);
        self.client_index = client_index;
        true
    }
}

pub(super) struct RunningTask {
    pub(super) id: u64,
    pub(super) segment_index: u32,
    pub(super) handle: tokio::task::JoinHandle<()>,
}

impl ExecutorState {
    pub(super) fn new(
        authority_keys: &[String],
        total_limit: usize,
        server_hard_limit: usize,
        session_count: usize,
    ) -> Self {
        let hard_limit = total_limit.max(1);
        let connection_limit = server_hard_limit.clamp(1, MAX_SERVER_CONCURRENCY);
        let session_count = session_count.max(1);
        let http1_client_slot_limit = hard_limit.div_ceil(session_count).max(1);
        let authorities = DashMap::new();
        for key in authority_keys {
            let client_warmups = DashMap::new();
            let primary_session_ready = Arc::new(OnceCell::new());
            let _ = primary_session_ready.set(Ok(()));
            client_warmups.insert(0, primary_session_ready);
            authorities.insert(
                key.clone().into_boxed_str(),
                Arc::new(AuthorityState {
                    hard_limit,
                    connection_limit,
                    target: AtomicUsize::new(hard_limit),
                    in_flight: AtomicUsize::new(0),
                    client_in_flight: Arc::new(
                        (0..session_count).map(|_| AtomicUsize::new(0)).collect(),
                    ),
                    http1_client_slot_limit,
                    next_client_index: AtomicUsize::new(0),
                    protocol: AtomicU8::new(0),
                    active_h2_sessions: AtomicUsize::new(1),
                    client_warmups,
                }),
            );
        }
        Self {
            authorities,
            total_in_flight: AtomicUsize::new(0),
        }
    }

    pub(super) fn authority(&self, authority_key: &str) -> Option<Arc<AuthorityState>> {
        self.authorities
            .get(authority_key)
            .map(|entry| Arc::clone(entry.value()))
    }

    pub(super) fn try_acquire(
        self: &Arc<Self>,
        authority: &Arc<AuthorityState>,
        total_limit: usize,
    ) -> Option<(AdmissionLease, usize)> {
        if !reserve_total(&self.total_in_flight, total_limit) {
            return None;
        }
        if !reserve_authority(authority) {
            self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let Some(client_index) = authority.reserve_client_slot() else {
            authority.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        };
        Some((
            AdmissionLease {
                state: Arc::clone(self),
                authority: Arc::clone(authority),
                client_in_flight: Arc::clone(&authority.client_in_flight),
                client_index,
            },
            client_index,
        ))
    }
}

impl AuthorityState {
    fn reserve_client_slot(&self) -> Option<usize> {
        let count = self.client_in_flight.len();
        let protocol = self.protocol.load(Ordering::Acquire);
        let session_count = if protocol == 2 {
            self.active_h2_sessions
                .load(Ordering::Acquire)
                .clamp(1, count)
        } else {
            count
        };
        let client_slot_limit = if protocol == 2 {
            self.target
                .load(Ordering::Acquire)
                .div_ceil(session_count)
                .max(1)
        } else {
            self.http1_client_slot_limit
        };
        let start = self.next_client_index.fetch_add(1, Ordering::Relaxed) % session_count;
        for offset in 0..session_count {
            let index = (start + offset) % session_count;
            if reserve_bounded(&self.client_in_flight[index], client_slot_limit) {
                return Some(index);
            }
        }
        None
    }

    pub(super) fn client_warmup(
        &self,
        client_index: usize,
    ) -> Arc<OnceCell<std::result::Result<(), Aria2Error>>> {
        self.client_warmups
            .entry(client_index)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }
}

fn reserve_total(total: &AtomicUsize, limit: usize) -> bool {
    let mut current = total.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match total.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn reserve_authority(authority: &AuthorityState) -> bool {
    let protocol = authority.protocol.load(Ordering::Acquire);
    let hard_limit = if protocol == 2 {
        authority.hard_limit
    } else {
        authority.connection_limit
    };
    let target = authority.target.load(Ordering::Acquire).min(hard_limit);
    let mut current = authority.in_flight.load(Ordering::Acquire);
    loop {
        if current >= target {
            return false;
        }
        match authority.in_flight.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn reserve_bounded(value: &AtomicUsize, limit: usize) -> bool {
    let mut current = value.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match value.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}
