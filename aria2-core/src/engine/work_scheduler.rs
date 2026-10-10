//! Protocol-neutral queue and attempt lifecycle for download work items.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WorkId(u64);

impl WorkId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

pub(crate) struct WorkItem<T> {
    id: WorkId,
    payload: T,
    /// Maximum total attempts. `0` means unlimited.
    max_attempts: u32,
}

impl<T> WorkItem<T> {
    pub(crate) fn new(id: WorkId, payload: T, max_attempts: u32) -> Self {
        Self {
            id,
            payload,
            max_attempts,
        }
    }
}

pub(crate) struct WorkLease<T> {
    item: WorkItem<T>,
    attempt: u32,
}

impl<T> WorkLease<T> {
    pub(crate) fn id(&self) -> WorkId {
        self.item.id
    }

    pub(crate) fn payload(&self) -> &T {
        &self.item.payload
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkState {
    Pending,
    Delayed,
    InFlight,
    Completed,
    Failed,
    Cancelled,
}

pub(crate) enum RetryOutcome<T> {
    Scheduled,
    Exhausted(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkScheduleError {
    DuplicateId(WorkId),
    InvalidLease(WorkId),
}

impl fmt::Display for WorkScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "duplicate work item {}", id.value()),
            Self::InvalidLease(id) => {
                write!(formatter, "work item {} has no active lease", id.value())
            }
        }
    }
}

impl std::error::Error for WorkScheduleError {}

struct ReadyWork<T> {
    item: WorkItem<T>,
    previous_attempts: u32,
}

struct DelayedWork<T> {
    item: WorkItem<T>,
    previous_attempts: u32,
    ready_at: Instant,
    sequence: u64,
}

impl<T> PartialEq for DelayedWork<T> {
    fn eq(&self, other: &Self) -> bool {
        self.ready_at == other.ready_at && self.sequence == other.sequence
    }
}

impl<T> Eq for DelayedWork<T> {}

impl<T> PartialOrd for DelayedWork<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for DelayedWork<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ready_at
            .cmp(&other.ready_at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

pub(crate) struct WorkScheduler<T> {
    ready: VecDeque<ReadyWork<T>>,
    delayed: BinaryHeap<Reverse<DelayedWork<T>>>,
    in_flight: HashMap<WorkId, u32>,
    active_ids: HashSet<WorkId>,
    #[cfg(test)]
    states: HashMap<WorkId, WorkState>,
    #[cfg(test)]
    failed_count: usize,
    next_sequence: u64,
    cancelled: bool,
}

impl<T> Default for WorkScheduler<T> {
    fn default() -> Self {
        Self {
            ready: VecDeque::new(),
            delayed: BinaryHeap::new(),
            in_flight: HashMap::new(),
            active_ids: HashSet::new(),
            #[cfg(test)]
            states: HashMap::new(),
            #[cfg(test)]
            failed_count: 0,
            next_sequence: 0,
            cancelled: false,
        }
    }
}

impl<T> WorkScheduler<T> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn enqueue(&mut self, item: WorkItem<T>) -> Result<(), WorkScheduleError> {
        if self.cancelled {
            return Err(WorkScheduleError::InvalidLease(item.id));
        }
        if !self.active_ids.insert(item.id) {
            return Err(WorkScheduleError::DuplicateId(item.id));
        }
        #[cfg(test)]
        self.states.insert(item.id, WorkState::Pending);
        self.ready.push_back(ReadyWork {
            item,
            previous_attempts: 0,
        });
        Ok(())
    }

    /// Admit queued work up to the caller's current concurrency ceiling.
    pub(crate) fn admit(&mut self, max_in_flight: usize, now: Instant) -> Vec<WorkLease<T>> {
        if self.cancelled || max_in_flight == 0 {
            return Vec::new();
        }
        let capacity = max_in_flight.saturating_sub(self.in_flight.len());
        let mut leases = Vec::with_capacity(capacity.min(self.ready.len()));
        for _ in 0..capacity {
            let Some(lease) = self.admit_one(max_in_flight, now) else {
                break;
            };
            leases.push(lease);
        }
        leases
    }

    pub(crate) fn admit_one(&mut self, max_in_flight: usize, now: Instant) -> Option<WorkLease<T>> {
        self.admit_one_where(max_in_flight, now, |_| true)
    }

    /// Admit the first pending item accepted by the protocol adapter's
    /// current eligibility rule while keeping queue ownership in the core.
    pub(crate) fn admit_one_where(
        &mut self,
        max_in_flight: usize,
        now: Instant,
        mut eligible: impl FnMut(&T) -> bool,
    ) -> Option<WorkLease<T>> {
        if self.cancelled || self.in_flight.len() >= max_in_flight {
            return None;
        }
        self.promote_ready(now);
        let ready_index = self
            .ready
            .iter()
            .position(|ready| eligible(&ready.item.payload))?;
        let ready = self.ready.remove(ready_index)?;
        let attempt = ready.previous_attempts.saturating_add(1);
        let id = ready.item.id;
        self.in_flight.insert(id, attempt);
        #[cfg(test)]
        self.states.insert(id, WorkState::InFlight);
        Some(WorkLease {
            item: ready.item,
            attempt,
        })
    }

    pub(crate) fn is_scheduled(&self, id: WorkId) -> bool {
        self.active_ids.contains(&id)
    }

    /// Complete an item that was satisfied by another eligible source while
    /// it was waiting in the ready or delayed queue.
    pub(crate) fn complete_pending(&mut self, id: WorkId) -> bool {
        if self.ready.iter().any(|ready| ready.item.id == id) {
            self.ready.retain(|ready| ready.item.id != id);
        } else if self
            .delayed
            .iter()
            .any(|Reverse(delayed)| delayed.item.id == id)
        {
            let mut retained = BinaryHeap::new();
            while let Some(Reverse(delayed)) = self.delayed.pop() {
                if delayed.item.id != id {
                    retained.push(Reverse(delayed));
                }
            }
            self.delayed = retained;
        } else {
            return false;
        }
        self.active_ids.remove(&id);
        #[cfg(test)]
        self.states.insert(id, WorkState::Completed);
        true
    }

    /// Discard queued work while preserving active leases.
    pub(crate) fn discard_pending(&mut self) {
        for ready in self.ready.drain(..) {
            self.active_ids.remove(&ready.item.id);
            #[cfg(test)]
            self.states.insert(ready.item.id, WorkState::Cancelled);
        }
        while let Some(Reverse(delayed)) = self.delayed.pop() {
            self.active_ids.remove(&delayed.item.id);
            #[cfg(test)]
            self.states.insert(delayed.item.id, WorkState::Cancelled);
        }
    }

    pub(crate) fn complete(&mut self, lease: WorkLease<T>) -> Result<(), WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        self.active_ids.remove(&lease.item.id);
        #[cfg(test)]
        self.states.insert(lease.item.id, WorkState::Completed);
        Ok(())
    }

    /// Keep a partially satisfied work item scheduled after a successful
    /// sub-work result. Successful sub-work does not consume retry budget.
    pub(crate) fn reschedule_after_success(
        &mut self,
        lease: WorkLease<T>,
    ) -> Result<(), WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        #[cfg(test)]
        self.states.insert(lease.id(), WorkState::Pending);
        self.ready.push_back(ReadyWork {
            item: lease.item,
            previous_attempts: lease.attempt.saturating_sub(1),
        });
        Ok(())
    }

    /// Retry an adapter-classified attempt without charging its retry budget,
    /// such as when a range is reduced or transport capacity is adapted.
    pub(crate) fn retry_without_consuming_attempt(
        &mut self,
        lease: WorkLease<T>,
    ) -> Result<(), WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        #[cfg(test)]
        self.states.insert(lease.id(), WorkState::Pending);
        self.ready.push_back(ReadyWork {
            item: lease.item,
            previous_attempts: lease.attempt.saturating_sub(1),
        });
        Ok(())
    }

    /// Reset retry history when the adapter moves the work to a new source.
    pub(crate) fn retry_with_fresh_attempt_budget(
        &mut self,
        lease: WorkLease<T>,
    ) -> Result<(), WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        #[cfg(test)]
        self.states.insert(lease.id(), WorkState::Pending);
        self.ready.push_back(ReadyWork {
            item: lease.item,
            previous_attempts: 0,
        });
        Ok(())
    }

    /// Retry a failed attempt or return its payload to the caller when the
    /// adapter has classified the error as terminal or attempts are exhausted.
    pub(crate) fn fail(
        &mut self,
        lease: WorkLease<T>,
        retry_at: Option<Instant>,
    ) -> Result<RetryOutcome<T>, WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        let id = lease.id();
        let retry_allowed = lease.item.max_attempts == 0 || lease.attempt < lease.item.max_attempts;
        if let Some(ready_at) = retry_at.filter(|_| retry_allowed && !self.cancelled) {
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1);
            self.delayed.push(Reverse(DelayedWork {
                item: lease.item,
                previous_attempts: lease.attempt,
                ready_at,
                sequence,
            }));
            #[cfg(test)]
            self.states.insert(id, WorkState::Delayed);
            Ok(RetryOutcome::Scheduled)
        } else {
            self.active_ids.remove(&id);
            #[cfg(test)]
            self.states.insert(id, WorkState::Failed);
            #[cfg(test)]
            {
                self.failed_count += 1;
            }
            Ok(RetryOutcome::Exhausted(lease.item.payload))
        }
    }

    /// Return an admitted item to the front of the queue when transport-level
    /// admission fails before the work itself has started.
    pub(crate) fn requeue_unstarted(
        &mut self,
        lease: WorkLease<T>,
    ) -> Result<(), WorkScheduleError> {
        self.remove_active_lease(&lease)?;
        #[cfg(test)]
        self.states.insert(lease.id(), WorkState::Pending);
        self.ready.push_front(ReadyWork {
            item: lease.item,
            previous_attempts: lease.attempt.saturating_sub(1),
        });
        Ok(())
    }

    pub(crate) fn next_retry_deadline(&self) -> Option<Instant> {
        self.delayed.peek().map(|entry| entry.0.ready_at)
    }

    #[cfg(test)]
    pub(crate) fn state(&self, id: WorkId) -> Option<WorkState> {
        self.states.get(&id).copied()
    }

    pub(crate) fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    #[cfg(test)]
    pub(crate) fn failed_count(&self) -> usize {
        self.failed_count
    }

    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        self.active_ids.is_empty()
    }

    /// Stop admitting and retrying work. The caller cancels active worker
    /// futures before reusing or dropping this scheduler.
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
        self.ready.clear();
        self.delayed.clear();
        self.in_flight.clear();
        self.active_ids.clear();
        #[cfg(test)]
        for state in self.states.values_mut() {
            if !matches!(state, WorkState::Completed | WorkState::Failed) {
                *state = WorkState::Cancelled;
            }
        }
    }

    fn remove_active_lease(&mut self, lease: &WorkLease<T>) -> Result<(), WorkScheduleError> {
        match self.in_flight.remove(&lease.item.id) {
            Some(attempt) if attempt == lease.attempt => Ok(()),
            Some(attempt) => {
                self.in_flight.insert(lease.item.id, attempt);
                Err(WorkScheduleError::InvalidLease(lease.item.id))
            }
            None => Err(WorkScheduleError::InvalidLease(lease.item.id)),
        }
    }

    fn promote_ready(&mut self, now: Instant) {
        while self
            .delayed
            .peek()
            .is_some_and(|entry| entry.0.ready_at <= now)
        {
            let Reverse(delayed) = self.delayed.pop().expect("peeked delayed item");
            #[cfg(test)]
            self.states.insert(delayed.item.id, WorkState::Pending);
            self.ready.push_back(ReadyWork {
                item: delayed.item,
                previous_attempts: delayed.previous_attempts,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn admission_respects_capacity_and_completion_releases_a_slot() {
        let now = Instant::now();
        let mut scheduler = WorkScheduler::new();
        for value in 1..=3 {
            scheduler
                .enqueue(WorkItem::new(WorkId::new(value), value, 3))
                .unwrap();
        }

        let mut admitted = scheduler.admit(2, now);
        assert_eq!(admitted.len(), 2);
        assert_eq!(scheduler.in_flight_count(), 2);
        let first = admitted.remove(0);
        scheduler.complete(first).unwrap();

        let next = scheduler.admit(2, now);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].payload(), &3);
    }

    #[test]
    fn retry_waits_until_deadline_and_exhausts_total_attempts() {
        let now = Instant::now();
        let id = WorkId::new(7);
        let mut scheduler = WorkScheduler::new();
        scheduler
            .enqueue(WorkItem::new(id, "fake payload", 2))
            .unwrap();

        let first = scheduler.admit(1, now).pop().unwrap();
        assert_eq!(first.attempt(), 1);
        assert!(matches!(
            scheduler
                .fail(first, Some(now + Duration::from_secs(2)))
                .unwrap(),
            RetryOutcome::Scheduled
        ));
        assert!(scheduler.admit(1, now + Duration::from_secs(1)).is_empty());
        assert_eq!(
            scheduler.next_retry_deadline(),
            Some(now + Duration::from_secs(2))
        );

        let second = scheduler
            .admit(1, now + Duration::from_secs(2))
            .pop()
            .unwrap();
        assert_eq!(second.attempt(), 2);
        assert!(matches!(
            scheduler
                .fail(second, Some(now + Duration::from_secs(3)))
                .unwrap(),
            RetryOutcome::Exhausted("fake payload")
        ));
        assert_eq!(scheduler.state(id), Some(WorkState::Failed));
        assert_eq!(scheduler.failed_count(), 1);
    }

    #[test]
    fn requeue_before_execution_does_not_consume_attempt_budget() {
        let now = Instant::now();
        let id = WorkId::new(9);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(id, 42, 1)).unwrap();

        let lease = scheduler.admit(1, now).pop().unwrap();
        scheduler.requeue_unstarted(lease).unwrap();
        let admitted = scheduler.admit(1, now).pop().unwrap();

        assert_eq!(admitted.attempt(), 1);
        assert_eq!(admitted.payload(), &42);
    }

    #[test]
    fn successful_sub_work_preserves_only_previously_failed_attempts() {
        let now = Instant::now();
        let id = WorkId::new(10);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(id, 42, 3)).unwrap();

        let first = scheduler.admit(1, now).pop().unwrap();
        scheduler
            .fail(first, Some(now))
            .expect("first attempt can retry");
        let second = scheduler.admit(1, now).pop().unwrap();
        scheduler.reschedule_after_success(second).unwrap();
        let third = scheduler.admit(1, now).pop().unwrap();

        assert_eq!(third.attempt(), 2);
        assert_eq!(third.payload(), &42);
    }

    #[test]
    fn adapter_classified_capacity_retry_does_not_consume_attempt_budget() {
        let now = Instant::now();
        let id = WorkId::new(12);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(id, 42, 1)).unwrap();

        let first = scheduler.admit(1, now).pop().unwrap();
        scheduler.retry_without_consuming_attempt(first).unwrap();
        let admitted = scheduler.admit(1, now).pop().unwrap();

        assert_eq!(admitted.attempt(), 1);
        assert_eq!(admitted.payload(), &42);
    }

    #[test]
    fn changing_source_starts_a_fresh_attempt_budget() {
        let now = Instant::now();
        let id = WorkId::new(13);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(id, 42, 2)).unwrap();

        let first = scheduler.admit(1, now).pop().unwrap();
        scheduler.fail(first, Some(now)).unwrap();
        let second = scheduler.admit(1, now).pop().unwrap();
        scheduler.retry_with_fresh_attempt_budget(second).unwrap();
        let admitted = scheduler.admit(1, now).pop().unwrap();

        assert_eq!(admitted.attempt(), 1);
        assert_eq!(admitted.payload(), &42);
    }

    #[test]
    fn duplicate_ids_are_rejected_and_cancel_prevents_new_admission() {
        let now = Instant::now();
        let mut scheduler = WorkScheduler::new();
        let id = WorkId::new(11);
        scheduler.enqueue(WorkItem::new(id, (), 1)).unwrap();
        assert_eq!(
            scheduler.enqueue(WorkItem::new(id, (), 1)),
            Err(WorkScheduleError::DuplicateId(id))
        );
        scheduler.cancel();
        assert!(scheduler.admit(1, now).is_empty());
        assert_eq!(scheduler.state(id), Some(WorkState::Cancelled));
        assert!(scheduler.is_finished());
    }

    #[test]
    fn adapter_eligibility_skips_a_blocked_item_without_reordering_it() {
        let now = Instant::now();
        let blocked = WorkId::new(21);
        let eligible = WorkId::new(22);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(blocked, false, 0)).unwrap();
        scheduler.enqueue(WorkItem::new(eligible, true, 0)).unwrap();

        let lease = scheduler
            .admit_one_where(1, now, |is_eligible| *is_eligible)
            .unwrap();

        assert_eq!(lease.id(), eligible);
        assert!(scheduler.is_scheduled(blocked));
        assert!(scheduler.is_scheduled(eligible));
    }

    #[test]
    fn externally_completed_pending_work_is_removed_from_ready_and_retry_queues() {
        let now = Instant::now();
        let ready_id = WorkId::new(31);
        let delayed_id = WorkId::new(32);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(delayed_id, (), 0)).unwrap();

        let lease = scheduler.admit(1, now).pop().unwrap();
        scheduler
            .fail(lease, Some(now + Duration::from_secs(10)))
            .unwrap();
        scheduler.enqueue(WorkItem::new(ready_id, (), 0)).unwrap();
        assert!(scheduler.complete_pending(ready_id));
        assert!(scheduler.complete_pending(delayed_id));
        assert_eq!(scheduler.state(ready_id), Some(WorkState::Completed));
        assert_eq!(scheduler.state(delayed_id), Some(WorkState::Completed));
        assert!(scheduler.is_finished());
    }

    #[test]
    fn terminal_work_ids_can_be_reused_without_retaining_scheduler_state() {
        let now = Instant::now();
        let id = WorkId::new(41);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(id, (), 1)).unwrap();
        let lease = scheduler.admit(1, now).pop().unwrap();
        scheduler.complete(lease).unwrap();

        assert!(!scheduler.is_scheduled(id));
        assert_eq!(scheduler.state(id), Some(WorkState::Completed));
        scheduler.enqueue(WorkItem::new(id, (), 1)).unwrap();
        assert!(scheduler.is_scheduled(id));
    }

    #[test]
    fn discarding_pending_work_preserves_active_leases() {
        let now = Instant::now();
        let active_id = WorkId::new(51);
        let pending_id = WorkId::new(52);
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue(WorkItem::new(active_id, (), 1)).unwrap();
        scheduler.enqueue(WorkItem::new(pending_id, (), 1)).unwrap();
        let lease = scheduler.admit(1, now).pop().unwrap();

        scheduler.discard_pending();

        assert!(scheduler.is_scheduled(active_id));
        assert!(!scheduler.is_scheduled(pending_id));
        assert_eq!(scheduler.in_flight_count(), 1);
        scheduler.complete(lease).unwrap();
        assert!(scheduler.is_finished());
    }
}
