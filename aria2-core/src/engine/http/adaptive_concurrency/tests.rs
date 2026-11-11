use super::*;

fn controller() -> HttpAdaptiveConcurrency {
    HttpAdaptiveConcurrency::new(16, 16, 4, 4, 0)
}

#[test]
fn starts_at_each_effective_ceiling_and_success_does_not_raise_it() {
    let mut controller = controller();
    assert_eq!(controller.connection_target(false), 16);
    assert_eq!(controller.range_target(false), 16);
    assert_eq!(controller.connection_target(true), 4);
    assert_eq!(controller.range_target(true), 16);

    for _ in 0..8 {
        assert!(controller.can_start(0, false));
        assert!(controller.can_start(0, true));
        assert_eq!(controller.finish_round(false), AdaptiveUpdate::default());
        assert_eq!(controller.finish_round(true), AdaptiveUpdate::default());
    }
    assert_eq!(controller.connection_target(false), 16);
    assert_eq!(controller.connection_target(true), 4);
}

#[test]
fn capacity_rejection_lowers_only_the_affected_protocol_target() {
    let mut controller = controller();
    controller.record_capacity_failure(false);
    assert!(!controller.can_start(0, false));
    assert_eq!(controller.finish_round(false).range_target, Some(12));
    assert_eq!(controller.connection_target(false), 12);
    assert_eq!(controller.connection_target(true), 4);

    controller.record_capacity_failure(true);
    assert_eq!(controller.finish_round(true).range_target, Some(12));
    assert_eq!(controller.range_target(true), 12);
}

#[test]
fn retry_budget_is_preserved_until_single_range() {
    let mut controller = controller();
    assert!(controller.preserve_retry_budget(false));
    assert!(controller.preserve_retry_budget(true));

    for expected in [12, 9, 7, 6, 5, 4, 3, 2, 1] {
        controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        controller.record_capacity_failure(false);
        assert_eq!(controller.finish_round(false).range_target, Some(expected));
    }
    assert_eq!(controller.connection_target(false), 1);
    assert!(!controller.preserve_retry_budget(false));

    for expected in [12, 9, 7, 6, 5, 4, 3, 2, 1] {
        controller.http2.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        controller.record_capacity_failure(true);
        controller.finish_round(true);
        assert_eq!(controller.range_target(true), expected);
    }
    assert_eq!(controller.connection_target(true), 1);
    assert_eq!(controller.range_target(true), 1);
    assert!(!controller.preserve_retry_budget(true));
}

#[test]
fn capacity_rejection_reduces_target_and_paces_future_starts() {
    let mut controller = controller();
    let wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 12);
    assert!(wait >= Duration::from_secs(20) && wait <= CAPACITY_BACKOFF_MAX);
    assert!(
        !controller.can_start(0, false),
        "new ranges wait out the backoff"
    );
    assert!(controller.cooldown_remaining().is_some());

    // The rest of the burst is coalesced into the active cooldown episode.
    controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 12);
    assert_eq!(controller.backoff_steps(), 1);

    assert_eq!(
        controller.record_capacity_rejection(false, Some(Duration::from_secs(90))),
        Duration::from_secs(90),
        "a longer Retry-After is honored"
    );
    assert_eq!(controller.connection_target(false), 12);
    assert_eq!(controller.connection_target(true), 4);
}

#[test]
fn configured_reduction_and_backoff_reach_the_controller() {
    let settings = HttpAdaptiveOptions {
        reduction_percent: 50,
        backoff_base_secs: 3,
        backoff_max_secs: 5,
        ..HttpAdaptiveOptions::default()
    };
    let mut controller = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);
    let wait = controller.record_capacity_rejection(false, None);

    assert_eq!(controller.range_target(false), 8);
    assert!(wait >= Duration::from_secs(3) && wait <= Duration::from_millis(4500));
}

#[test]
fn backoff_persists_across_successes_and_resets_after_a_quiet_window() {
    let mut controller = HttpAdaptiveConcurrency::new(16, 3, 4, 4, 0);
    let first_wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 2);
    assert_eq!(controller.backoff_steps(), 1);
    assert!(first_wait >= Duration::from_secs(20));
    assert!(first_wait <= Duration::from_secs(30));

    // Model one successful Range between every capacity rejection. These
    // successes must not restart the retry delay at step one.
    for step in 2..=6 {
        controller.record_success(false);
        controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        let wait = controller.record_capacity_rejection(false, None);
        assert_eq!(controller.backoff_steps(), step);
        let base = Duration::from_secs((20_u64 << (step - 1)).min(300));
        assert!(wait >= base.mul_f64(0.5), "step {step}: {wait:?}");
        assert!(
            wait <= base.mul_f64(1.5).min(CAPACITY_BACKOFF_MAX),
            "step {step}: {wait:?}"
        );
    }

    controller.last_capacity_rejection =
        Some(Instant::now() - CAPACITY_BACKOFF_RESET_AFTER - Duration::from_millis(1));
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    let wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.backoff_steps(), 1);
    assert!(wait >= Duration::from_secs(20) && wait <= Duration::from_secs(30));
    assert_eq!(controller.connection_target(false), 1);
    assert_eq!(controller.range_target(false), 1);
}

#[test]
fn recovery_probe_waits_for_stability_and_cooldown_then_adds_one_range() {
    let mut controller = controller();
    controller.record_capacity_failure(false);
    assert_eq!(controller.finish_round(false).range_target, Some(12));
    controller.http1.last_target_adjustment =
        Instant::now() - RECOVERY_PROBE_INTERVAL - Duration::from_millis(1);
    controller.http1.cooldown_until = Some(Instant::now() + Duration::from_secs(1));

    for _ in 0..15 {
        controller.record_success(false);
    }
    assert_eq!(controller.finish_round(false), AdaptiveUpdate::default());
    assert_eq!(controller.range_target(false), 12);

    controller.record_failure(false);
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    controller.http1.last_target_adjustment =
        Instant::now() - RECOVERY_PROBE_INTERVAL - Duration::from_millis(1);
    for _ in 0..15 {
        controller.record_success(false);
    }

    let update = controller.finish_round(false);
    assert_eq!(update.range_target, Some(13));
    assert!(update.increased);
    assert_eq!(controller.range_target(false), 13);
}

#[test]
fn capacity_backoff_doubles_to_five_minutes_with_jitter() {
    for (step, base) in [(1, 20), (2, 40), (3, 80), (5, 300), (6, 300), (20, 300)] {
        let wait = capacity_backoff(step);
        let base = Duration::from_secs(base);
        assert!(
            wait >= base && wait <= base.mul_f64(1.5).min(CAPACITY_BACKOFF_MAX),
            "step {step}: {wait:?}"
        );
        assert!(wait <= CAPACITY_BACKOFF_MAX, "step {step}: {wait:?}");
    }
}

#[test]
fn split_budget_caps_ranges_without_changing_protocol_targets() {
    let mut controller = HttpAdaptiveConcurrency::new(3, 16, 4, 4, 0);
    assert_eq!(controller.range_target(false), 3);
    assert_eq!(controller.range_target(true), 3);
    assert!(!controller.can_start(3, false));
    assert!(!controller.can_start(3, true));
    assert_eq!(controller.connection_target(false), 3);
    assert_eq!(controller.connection_target(true), 1);
}

#[test]
fn recovery_threshold_scales_from_eight_to_thirty_two_and_caps() {
    let mut controller = HttpAdaptiveConcurrency::new(256, 256, 4, 64, 0);
    for (target, expected) in [
        (5, 8),
        (8, 8),
        (10, 10),
        (16, 16),
        (32, 32),
        (128, 32),
        (256, 32),
    ] {
        controller.http1.target = target;
        assert_eq!(controller.recovery_success_threshold(false), expected);
    }
}

#[test]
fn temporary_failure_does_not_lower_global_target() {
    let mut controller = controller();
    controller.record_failure(false);
    assert_eq!(controller.range_target(false), 16);
    assert_eq!(
        controller.http1.request_start_interval,
        INITIAL_REQUEST_START_INTERVAL
    );
}

#[test]
fn capacity_feedback_paces_then_successes_gradually_relax_start_rate() {
    let mut controller = controller();
    controller.record_capacity_rejection(false, None);
    assert_eq!(
        controller.http1.request_start_interval,
        CAPACITY_REQUEST_START_INTERVAL
    );

    for _ in 0..RATE_RECOVERY_SUCCESS_THRESHOLD {
        controller.record_success(false);
    }
    assert_eq!(
        controller.http1.request_start_interval,
        Duration::from_millis(900)
    );
}

#[test]
fn request_starts_obey_the_feedback_interval() {
    let mut controller = controller();
    controller.record_capacity_rejection(false, None);
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    controller.http1.last_request_started = Some(Instant::now());
    assert!(!controller.can_start(0, false));

    controller.http1.last_request_started =
        Some(Instant::now() - CAPACITY_REQUEST_START_INTERVAL - Duration::from_millis(1));
    assert!(controller.can_start(0, false));
}
