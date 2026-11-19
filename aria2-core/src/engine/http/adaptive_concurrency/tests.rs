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
    controller.record_capacity_failure(false, None);
    assert!(!controller.can_start(0, false));
    assert_eq!(controller.finish_round(false).range_target, Some(8));
    assert_eq!(controller.connection_target(false), 8);
    assert_eq!(controller.connection_target(true), 4);

    controller.record_capacity_failure(true, None);
    assert_eq!(controller.finish_round(true).range_target, Some(8));
    assert_eq!(controller.range_target(true), 8);
}

#[test]
fn repeated_capacity_rejections_reduce_each_protocol_target_to_one() {
    let mut controller = controller();
    for expected in [
        Some(8),
        Some(4),
        Some(2),
        Some(1),
        None,
        None,
        None,
        None,
        None,
    ] {
        controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        controller.record_capacity_failure(false, None);
        assert_eq!(controller.finish_round(false).range_target, expected);
    }
    assert_eq!(controller.connection_target(false), 1);

    for expected in [
        Some(8),
        Some(4),
        Some(2),
        Some(1),
        None,
        None,
        None,
        None,
        None,
    ] {
        controller.http2.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        controller.record_capacity_failure(true, None);
        assert_eq!(controller.finish_round(true).range_target, expected);
    }
    assert_eq!(controller.connection_target(true), 1);
    assert_eq!(controller.range_target(true), 1);
}

#[test]
fn capacity_rejection_reduces_target_and_applies_cooldown() {
    let mut controller = controller();
    let wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 8);
    assert!(wait >= Duration::from_secs(20) && wait <= CAPACITY_BACKOFF_MAX);
    assert!(
        !controller.can_start(0, false),
        "new ranges wait out the backoff"
    );
    assert!(controller.cooldown_remaining().is_some());

    controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 8);
    assert_eq!(controller.backoff_steps(), 1);

    assert_eq!(
        controller.record_capacity_rejection(false, Some(Duration::from_secs(90))),
        Duration::from_secs(90),
        "a longer Retry-After is honored"
    );
    assert_eq!(controller.connection_target(false), 8);
    assert_eq!(controller.connection_target(true), 4);
}

#[test]
fn capacity_cooldown_is_scoped_to_the_rejected_protocol() {
    let mut controller = controller();
    controller.record_capacity_failure(false, None);

    assert!(controller.cooldown_remaining_for(false).is_some());
    assert!(controller.cooldown_remaining_for(true).is_none());
}

#[test]
fn configured_reduction_and_backoff_reach_the_controller() {
    let settings = HttpAdaptiveOptions {
        enabled: true,
        reduction_percent: 50,
        backoff_base_secs: 3,
        backoff_max_secs: 5,
    };
    let mut controller = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);
    let wait = controller.record_capacity_rejection(false, None);

    assert_eq!(controller.range_target(false), 8);
    assert!(wait >= Duration::from_secs(3) && wait <= Duration::from_millis(4500));
}

#[test]
fn disabled_adaptive_mode_keeps_target_fixed_without_adaptive_backoff() {
    let settings = HttpAdaptiveOptions {
        enabled: false,
        ..HttpAdaptiveOptions::default()
    };
    let mut controller = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);

    let first_wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.connection_target(false), 16);
    assert_eq!(controller.range_target(false), 16);
    assert!(first_wait.is_zero());
    assert!(controller.can_start(0, false));

    controller.record_capacity_failure(false, None);
    let update = controller.finish_round(false);
    assert_eq!(update, AdaptiveUpdate::default());
    assert_eq!(controller.connection_target(false), 16);
    assert!(controller.can_start(0, false));

    controller.record_failure(false);
    for _ in 0..64 {
        controller.record_success(false);
    }
    assert_eq!(controller.finish_round(false), AdaptiveUpdate::default());
    assert_eq!(controller.connection_target(false), 16);
}

#[test]
fn healthy_range_starts_are_immediate() {
    let settings = HttpAdaptiveOptions::default();
    let mut controller = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);

    assert!(controller.can_start(0, false));
    assert!(controller.can_start(1, false));
}

#[test]
fn disabled_adaptive_mode_honors_explicit_retry_after() {
    let settings = HttpAdaptiveOptions {
        enabled: false,
        ..HttpAdaptiveOptions::default()
    };
    let mut controller = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);

    let wait = controller.record_capacity_rejection(false, Some(Duration::from_secs(3)));

    assert_eq!(wait, Duration::from_secs(3));
    assert!(!controller.can_start(0, false));
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    assert!(controller.can_start(0, false));

    let mut multi_round = HttpAdaptiveConcurrency::new_with_settings(16, 16, 4, 4, 0, settings);
    multi_round.record_capacity_failure(false, Some(Duration::from_secs(3)));
    assert!(!multi_round.can_start(0, false));
    assert_eq!(multi_round.finish_round(false), AdaptiveUpdate::default());
    assert_eq!(multi_round.connection_target(false), 16);
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
        Some(Instant::now() - BACKOFF_RESET_AFTER - Duration::from_millis(1));
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    let wait = controller.record_capacity_rejection(false, None);
    assert_eq!(controller.backoff_steps(), 1);
    assert!(wait >= Duration::from_secs(20) && wait <= Duration::from_secs(30));
    assert_eq!(controller.connection_target(false), 1);
    assert_eq!(controller.range_target(false), 1);
}

#[test]
fn recovery_waits_for_eight_successful_ranges_then_adds_one_range() {
    let mut controller = controller();
    controller.record_capacity_failure(false, None);
    assert_eq!(controller.finish_round(false).range_target, Some(8));
    controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));

    for _ in 0..7 {
        controller.record_success(false);
    }
    assert_eq!(controller.finish_round(false), AdaptiveUpdate::default());
    assert_eq!(controller.range_target(false), 8);

    controller.record_success(false);

    let update = controller.finish_round(false);
    assert_eq!(update.range_target, Some(9));
    assert!(update.increased);
    assert_eq!(controller.range_target(false), 9);
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
fn temporary_failure_does_not_lower_global_target() {
    let mut controller = controller();
    controller.record_failure(false);
    assert_eq!(controller.range_target(false), 16);
}
