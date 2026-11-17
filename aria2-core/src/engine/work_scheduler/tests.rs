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
