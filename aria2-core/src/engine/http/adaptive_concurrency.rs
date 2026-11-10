//! Capacity-only concurrency control for HTTP segmented downloads.
//!
//! Each protocol starts at the highest Range concurrency allowed by its
//! configured limits. An explicit server-capacity rejection stops new
//! admissions until the active round drains, then lowers the target and retries
//! the rejected range. After a cooldown and a stable window of successful
//! requests, the controller probes upward by one Range. Configured connection,
//! session, stream, and split limits remain hard ceilings and are never rewritten.
//!
//! Capacity feedback lowers the Range target multiplicatively and paces future starts.
//! Temporary no-progress retries remain local to a Range and do not alter the
//! global target.

use crate::request::request_group::HttpAdaptiveOptions;
use rand::RngExt;
use std::time::{Duration, Instant};

#[cfg(test)]
const RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const INITIAL_REQUEST_START_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(test)]
const CAPACITY_REQUEST_START_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const RATE_RECOVERY_SUCCESS_THRESHOLD: usize = 64;
#[cfg(test)]
const CAPACITY_BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AdaptiveUpdate {
    pub range_target: Option<usize>,
    pub increased: bool,
}

#[derive(Debug)]
struct ProtocolTarget {
    ceiling: usize,
    target: usize,
    capacity_rejected: bool,
    accepting_new_work: bool,
    cooldown_until: Option<Instant>,
    successful_ranges_since_adjustment: usize,
    last_target_adjustment: Instant,
    request_start_interval: Duration,
    last_request_started: Option<Instant>,
    capacity_feedback_received: bool,
    successful_ranges_since_rate_adjustment: usize,
}

impl ProtocolTarget {
    fn new(ceiling: usize, initial_request_start_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            ceiling: ceiling.max(1),
            target: ceiling.max(1),
            capacity_rejected: false,
            accepting_new_work: true,
            cooldown_until: None,
            successful_ranges_since_adjustment: 0,
            last_target_adjustment: now,
            request_start_interval: initial_request_start_interval,
            last_request_started: None,
            capacity_feedback_received: false,
            successful_ranges_since_rate_adjustment: 0,
        }
    }
}

/// Adaptive Range concurrency for one HTTP source. HTTP/1.1 Range concurrency
/// is bounded by physical connections. HTTP/2 Range concurrency is bounded by
/// the split budget and the configured session/stream product; the controller
/// derives the number of sessions needed for its current Range target.
#[derive(Debug)]
pub(crate) struct HttpAdaptiveConcurrency {
    settings: HttpAdaptiveOptions,
    split_budget: usize,
    http1: ProtocolTarget,
    http2: ProtocolTarget,
    http2_streams_per_session: usize,
    retry_wait: Duration,
    /// Backoff episodes since the last sustained 429-free interval.
    backoff_steps: u32,
    last_capacity_rejection: Option<Instant>,
}

#[cfg(test)]
const CAPACITY_BACKOFF_MAX: Duration = Duration::from_secs(300);

#[cfg(test)]
fn capacity_backoff(step: u32) -> Duration {
    capacity_backoff_with(step, &HttpAdaptiveOptions::default())
}

/// Compute the exponential wait for the `step`th capacity retry, with positive
/// jitter. The server's `Retry-After` value is applied by the caller.
pub(crate) fn capacity_backoff_with(step: u32, settings: &HttpAdaptiveOptions) -> Duration {
    let base = Duration::from_secs(settings.backoff_base_secs)
        .saturating_mul(1 << step.saturating_sub(1).min(31))
        .min(Duration::from_secs(settings.backoff_max_secs));
    base.mul_f64(rand::rng().random_range(1.0..1.5))
        .min(Duration::from_secs(settings.backoff_max_secs))
}

impl HttpAdaptiveConcurrency {
    #[cfg(test)]
    pub(crate) fn new(
        split_budget: usize,
        http1_connection_limit: usize,
        http2_session_limit: usize,
        http2_streams_per_session: usize,
        retry_wait_secs: u64,
    ) -> Self {
        Self::new_with_settings(
            split_budget,
            http1_connection_limit,
            http2_session_limit,
            http2_streams_per_session,
            retry_wait_secs,
            HttpAdaptiveOptions::default(),
        )
    }

    pub(crate) fn new_with_settings(
        split_budget: usize,
        http1_connection_limit: usize,
        http2_session_limit: usize,
        http2_streams_per_session: usize,
        retry_wait_secs: u64,
        settings: HttpAdaptiveOptions,
    ) -> Self {
        let settings = settings.normalized();
        let initial_interval = Duration::from_millis(settings.start_interval_ms);
        Self {
            settings,
            split_budget: split_budget.max(1),
            http1: ProtocolTarget::new(
                http1_connection_limit.min(split_budget.max(1)),
                initial_interval,
            ),
            http2: ProtocolTarget::new(
                split_budget.max(1).min(
                    http2_session_limit
                        .max(1)
                        .saturating_mul(http2_streams_per_session.max(1)),
                ),
                initial_interval,
            ),
            http2_streams_per_session: http2_streams_per_session.max(1),
            retry_wait: Duration::from_secs(retry_wait_secs),
            backoff_steps: 0,
            last_capacity_rejection: None,
        }
    }

    pub(crate) fn connection_target(&self, is_http2: bool) -> usize {
        let target = self.protocol_target(is_http2).target;
        if is_http2 {
            target.div_ceil(self.http2_streams_per_session)
        } else {
            target
        }
    }

    pub(crate) fn range_target(&self, is_http2: bool) -> usize {
        self.protocol_target(is_http2)
            .target
            .min(self.split_budget)
            .max(1)
    }

    fn recovery_success_threshold(&self, is_http2: bool) -> usize {
        self.protocol_target(is_http2).target.clamp(
            self.settings.recovery_successes_min as usize,
            self.settings.recovery_successes_max as usize,
        )
    }

    fn reduce_target(target: usize, reduction_percent: u8) -> usize {
        target
            .saturating_mul(100 - reduction_percent as usize)
            .div_ceil(100)
            .min(target.saturating_sub(1).max(1))
            .max(1)
    }

    /// Whether another range can start within the current protocol target.
    pub(crate) fn can_start(&mut self, active: usize, is_http2: bool) -> bool {
        let now = Instant::now();
        let range_target = self.range_target(is_http2);
        let protocol = self.protocol_target_mut(is_http2);
        if !protocol.accepting_new_work || active >= range_target {
            return false;
        }
        if let Some(until) = protocol.cooldown_until {
            if now < until {
                return false;
            }
            protocol.cooldown_until = None;
        }
        protocol.last_request_started.is_none_or(|last_started| {
            now.duration_since(last_started) >= protocol.request_start_interval
        })
    }

    /// Record an admitted request so the next Range for this authority can be
    /// paced independently from its concurrent-request ceiling.
    pub(crate) fn record_request_start(&mut self, is_http2: bool) {
        self.protocol_target_mut(is_http2).last_request_started = Some(Instant::now());
    }

    fn request_start_wait(&self, is_http2: bool) -> Option<Duration> {
        let protocol = self.protocol_target(is_http2);
        let last_started = protocol.last_request_started?;
        let allowed_at = last_started.checked_add(protocol.request_start_interval)?;
        let remaining = allowed_at.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then_some(remaining)
    }

    fn note_capacity_feedback(&mut self, is_http2: bool) {
        let settings = self.settings;
        let protocol = self.protocol_target_mut(is_http2);
        protocol.request_start_interval = if !protocol.capacity_feedback_received {
            Duration::from_millis(settings.capacity_start_interval_ms)
        } else {
            protocol
                .request_start_interval
                .saturating_mul(2)
                .min(Duration::from_millis(
                    settings.capacity_start_interval_max_ms,
                ))
        };
        protocol.capacity_feedback_received = true;
        protocol.successful_ranges_since_rate_adjustment = 0;
    }

    /// Freeze new admissions after an explicit server capacity rejection.
    pub(crate) fn record_capacity_failure(&mut self, is_http2: bool) {
        let settings = self.settings;
        let now = Instant::now();
        let in_cooldown = self
            .protocol_target(is_http2)
            .cooldown_until
            .is_some_and(|until| now < until);
        let protocol = self.protocol_target_mut(is_http2);
        let new_episode = !protocol.capacity_rejected && !in_cooldown;
        protocol.capacity_rejected |= new_episode;
        protocol.accepting_new_work = false;
        protocol.successful_ranges_since_adjustment = 0;
        if new_episode {
            if self.last_capacity_rejection.is_some_and(|last| {
                now.duration_since(last) >= Duration::from_secs(settings.backoff_reset_after_secs)
            }) {
                self.backoff_steps = 0;
            }
            self.last_capacity_rejection = Some(now);
            self.backoff_steps = self.backoff_steps.saturating_add(1);
            self.note_capacity_feedback(is_http2);
            let wait = self
                .retry_wait
                .max(capacity_backoff_with(self.backoff_steps, &settings));
            let until = now.checked_add(wait).unwrap_or(now);
            let protocol = self.protocol_target_mut(is_http2);
            protocol.cooldown_until =
                Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
        }
    }

    /// A 429/503 pauses new work, lowers concurrency by about 25% per rejection
    /// episode (at least one Range), and slows subsequent request starts.
    /// Ranges in flight continue.
    /// Returns a conservative backoff, extended by Retry-After when present.
    pub(crate) fn record_capacity_rejection(
        &mut self,
        is_http2: bool,
        retry_after: Option<Duration>,
    ) -> Duration {
        let settings = self.settings;
        let now = Instant::now();
        if self.last_capacity_rejection.is_some_and(|last| {
            now.duration_since(last) >= Duration::from_secs(settings.backoff_reset_after_secs)
        }) {
            self.backoff_steps = 0;
        }
        self.last_capacity_rejection = Some(now);
        let protocol = self.protocol_target_mut(is_http2);
        let in_cooldown = protocol.cooldown_until.is_some_and(|until| now < until);
        if !in_cooldown {
            protocol.target = Self::reduce_target(protocol.target, settings.reduction_percent);
            self.note_capacity_feedback(is_http2);
        }
        let protocol = self.protocol_target_mut(is_http2);
        protocol.successful_ranges_since_adjustment = 0;
        protocol.successful_ranges_since_rate_adjustment = 0;
        protocol.last_target_adjustment = now;
        // One backoff step per episode: the other ranges of the same burst
        // are rejected inside the cooldown it started.
        if !in_cooldown {
            self.backoff_steps = self.backoff_steps.saturating_add(1);
        }
        let wait = retry_after
            .unwrap_or_default()
            .max(capacity_backoff_with(self.backoff_steps, &self.settings));
        let until = now.checked_add(wait).unwrap_or(now);
        let protocol = self.protocol_target_mut(is_http2);
        protocol.cooldown_until = Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
        wait
    }

    /// Record a successful range for concurrency recovery. A single successful
    /// range does not clear capacity backoff history.
    pub(crate) fn record_success(&mut self, is_http2: bool) {
        let settings = self.settings;
        let protocol = self.protocol_target_mut(is_http2);
        protocol.successful_ranges_since_adjustment = protocol
            .successful_ranges_since_adjustment
            .saturating_add(1);
        protocol.successful_ranges_since_rate_adjustment = protocol
            .successful_ranges_since_rate_adjustment
            .saturating_add(1);
        if protocol.successful_ranges_since_rate_adjustment
            >= settings.rate_recovery_successes as usize
            && !protocol.request_start_interval.is_zero()
        {
            protocol.request_start_interval = protocol
                .request_start_interval
                .mul_f64(0.9)
                .max(Duration::from_millis(settings.rate_min_interval_ms));
            protocol.successful_ranges_since_rate_adjustment = 0;
        }
    }

    /// Reset the recovery window after any failed Range request.
    pub(crate) fn record_failure(&mut self, is_http2: bool) {
        self.protocol_target_mut(is_http2)
            .successful_ranges_since_adjustment = 0;
        self.protocol_target_mut(is_http2)
            .successful_ranges_since_rate_adjustment = 0;
    }

    /// Backoff episodes since the last sustained 429-free interval.
    pub(crate) fn backoff_steps(&self) -> u32 {
        self.backoff_steps
    }

    /// Capacity rejections can be retried without consuming ordinary retry
    /// budget while there is still a lower Range target to try.
    pub(crate) fn preserve_retry_budget(&self, is_http2: bool) -> bool {
        self.range_target(is_http2) > 1
    }

    /// Close the current round after all requests have drained. Capacity
    /// rejection lowers the Range target by about 25% and returns the new target.
    pub(crate) fn finish_round(&mut self, is_http2: bool) -> AdaptiveUpdate {
        let retry_wait = self.retry_wait;
        let settings = self.settings;
        let success_threshold = self.recovery_success_threshold(is_http2);
        let protocol = self.protocol_target_mut(is_http2);
        let now = Instant::now();
        let (changed, increased) = if protocol.capacity_rejected {
            let reduced_target = Self::reduce_target(protocol.target, settings.reduction_percent);
            let changed = if reduced_target < protocol.target {
                protocol.target = reduced_target;
                Some(protocol.target)
            } else {
                None
            };
            protocol.last_target_adjustment = now;
            protocol.successful_ranges_since_adjustment = 0;
            if !retry_wait.is_zero() {
                let until = now + retry_wait;
                protocol.cooldown_until =
                    Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
            }
            (changed, false)
        } else if protocol.target < protocol.ceiling
            && protocol.successful_ranges_since_adjustment >= success_threshold
            && protocol.cooldown_until.is_none_or(|until| now >= until)
            && now.duration_since(protocol.last_target_adjustment)
                >= Duration::from_secs(settings.recovery_interval_secs)
        {
            protocol.target += 1;
            protocol.successful_ranges_since_adjustment = 0;
            protocol.last_target_adjustment = now;
            (Some(protocol.target), true)
        } else {
            (None, false)
        };
        protocol.capacity_rejected = false;
        protocol.accepting_new_work = true;
        AdaptiveUpdate {
            range_target: changed,
            increased,
        }
    }

    pub(crate) fn cooldown_remaining(&self) -> Option<Duration> {
        [false, true]
            .into_iter()
            .flat_map(|is_http2| {
                [
                    self.protocol_target(is_http2)
                        .cooldown_until
                        .map(|until| until.saturating_duration_since(Instant::now())),
                    self.request_start_wait(is_http2),
                ]
            })
            .flatten()
            .filter(|remaining| !remaining.is_zero())
            .max()
    }

    fn protocol_target(&self, is_http2: bool) -> &ProtocolTarget {
        if is_http2 { &self.http2 } else { &self.http1 }
    }

    fn protocol_target_mut(&mut self, is_http2: bool) -> &mut ProtocolTarget {
        if is_http2 {
            &mut self.http2
        } else {
            &mut self.http1
        }
    }
}

#[cfg(test)]
mod tests {
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
}
