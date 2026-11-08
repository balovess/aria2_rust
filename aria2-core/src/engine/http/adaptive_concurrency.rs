//! Capacity-only concurrency control for HTTP segmented downloads.
//!
//! Each protocol starts at the highest Range concurrency allowed by its
//! configured limits. An explicit server-capacity rejection stops new
//! admissions until the active round drains, then lowers the target and retries
//! the rejected range. After a cooldown and a stable window of successful
//! requests, the controller probes upward by one Range. Configured connection,
//! session, stream, and split limits remain hard ceilings and are never rewritten.
//!
//! A single-source download instead treats a 429/503 as a rate limit: the
//! target drops at once to the ranges the server is still serving, and new
//! ranges pause for Retry-After (or an exponential backoff).

use rand::RngExt;
use std::time::{Duration, Instant};

const RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_PROBE_SUCCESS_THRESHOLD: usize = 8;

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
}

impl ProtocolTarget {
    fn new(ceiling: usize) -> Self {
        let now = Instant::now();
        Self {
            ceiling: ceiling.max(1),
            target: ceiling.max(1),
            capacity_rejected: false,
            accepting_new_work: true,
            cooldown_until: None,
            successful_ranges_since_adjustment: 0,
            last_target_adjustment: now,
        }
    }
}

/// Adaptive Range concurrency for one HTTP source. HTTP/1.1 Range concurrency
/// is bounded by physical connections. HTTP/2 Range concurrency is bounded by
/// the split budget and the configured session/stream product; the controller
/// derives the number of sessions needed for its current Range target.
#[derive(Debug)]
pub(crate) struct HttpAdaptiveConcurrency {
    split_budget: usize,
    http1: ProtocolTarget,
    http2: ProtocolTarget,
    http2_streams_per_session: usize,
    retry_wait: Duration,
    /// Backoff episodes since the last successful range.
    backoff_steps: u32,
}

const CAPACITY_BACKOFF_BASE: Duration = Duration::from_secs(1);
const CAPACITY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Wait before the `step`th retry (from 1) after a 429/503 without
/// Retry-After: 1 s doubling to 30 s, with +-50% jitter so parallel
/// downloads do not retry in lockstep.
pub(crate) fn capacity_backoff(step: u32) -> Duration {
    let base = CAPACITY_BACKOFF_BASE
        .saturating_mul(1 << step.saturating_sub(1).min(5))
        .min(CAPACITY_BACKOFF_MAX);
    base.mul_f64(rand::rng().random_range(0.5..1.5))
}

impl HttpAdaptiveConcurrency {
    pub(crate) fn new(
        split_budget: usize,
        http1_connection_limit: usize,
        http2_session_limit: usize,
        http2_streams_per_session: usize,
        retry_wait_secs: u64,
    ) -> Self {
        Self {
            split_budget: split_budget.max(1),
            http1: ProtocolTarget::new(http1_connection_limit.min(split_budget.max(1))),
            http2: ProtocolTarget::new(
                split_budget.max(1).min(
                    http2_session_limit
                        .max(1)
                        .saturating_mul(http2_streams_per_session.max(1)),
                ),
            ),
            http2_streams_per_session: http2_streams_per_session.max(1),
            retry_wait: Duration::from_secs(retry_wait_secs),
            backoff_steps: 0,
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

    /// Whether another range can start within the current protocol target.
    pub(crate) fn can_start(&mut self, active: usize, is_http2: bool) -> bool {
        let range_target = self.range_target(is_http2);
        let protocol = self.protocol_target_mut(is_http2);
        if !protocol.accepting_new_work || active >= range_target {
            return false;
        }
        if let Some(until) = protocol.cooldown_until {
            if Instant::now() < until {
                return false;
            }
            protocol.cooldown_until = None;
        }
        true
    }

    /// Freeze new admissions after an explicit server capacity rejection.
    pub(crate) fn record_capacity_failure(&mut self, is_http2: bool) {
        let protocol = self.protocol_target_mut(is_http2);
        protocol.capacity_rejected = true;
        protocol.accepting_new_work = false;
        protocol.successful_ranges_since_adjustment = 0;
    }

    /// A 429/503 while `accepted` other ranges are being served: the server
    /// takes at most that many, so drop to it at once (by at least one), and
    /// pause new ranges for `retry_after`, or else an exponential backoff.
    /// Ranges in flight keep going. Returns the wait.
    pub(crate) fn record_capacity_rejection(
        &mut self,
        is_http2: bool,
        accepted: usize,
        retry_after: Option<Duration>,
    ) -> Duration {
        let now = Instant::now();
        let protocol = self.protocol_target_mut(is_http2);
        let in_cooldown = protocol.cooldown_until.is_some_and(|until| now < until);
        // A new episode lowers by at least one; the rest of its burst only
        // to what the server is serving.
        let lowered = if in_cooldown {
            protocol.target
        } else {
            protocol.target.saturating_sub(1)
        };
        protocol.target = lowered.min(accepted).max(1);
        protocol.successful_ranges_since_adjustment = 0;
        protocol.last_target_adjustment = now;
        // One backoff step per episode: the other ranges of the same burst
        // are rejected inside the cooldown it started.
        if !in_cooldown {
            self.backoff_steps = self.backoff_steps.saturating_add(1);
        }
        let wait = retry_after.unwrap_or_else(|| capacity_backoff(self.backoff_steps));
        let until = now.checked_add(wait).unwrap_or(now);
        let protocol = self.protocol_target_mut(is_http2);
        protocol.cooldown_until = Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
        wait
    }

    /// A successful range clears backoff history. The target remains at the
    /// reduced value for this download; new downloads begin at their ceiling.
    pub(crate) fn record_success(&mut self, is_http2: bool) {
        self.backoff_steps = 0;
        let protocol = self.protocol_target_mut(is_http2);
        protocol.successful_ranges_since_adjustment =
            protocol.successful_ranges_since_adjustment.saturating_add(1);
    }

    /// Reset the recovery window after any failed Range request.
    pub(crate) fn record_failure(&mut self, is_http2: bool) {
        self.protocol_target_mut(is_http2)
            .successful_ranges_since_adjustment = 0;
    }

    /// Backoff episodes since the last successful range.
    pub(crate) fn backoff_steps(&self) -> u32 {
        self.backoff_steps
    }

    /// Capacity rejections can be retried without consuming ordinary retry
    /// budget while there is still a lower Range target to try.
    pub(crate) fn preserve_retry_budget(&self, is_http2: bool) -> bool {
        self.range_target(is_http2) > 1
    }

    /// Close the current round after all requests have drained. Capacity
    /// rejection lowers the Range target by one and returns the new target.
    pub(crate) fn finish_round(&mut self, is_http2: bool) -> AdaptiveUpdate {
        let retry_wait = self.retry_wait;
        let protocol = self.protocol_target_mut(is_http2);
        let now = Instant::now();
        let (changed, increased) = if protocol.capacity_rejected {
            let changed = if protocol.target > 1 {
                protocol.target -= 1;
                Some(protocol.target)
            } else {
                None
            };
            protocol.last_target_adjustment = now;
            protocol.successful_ranges_since_adjustment = 0;
            if !retry_wait.is_zero() {
                protocol.cooldown_until = Some(now + retry_wait);
            }
            (changed, false)
        } else if protocol.target < protocol.ceiling
            && protocol.successful_ranges_since_adjustment >= RECOVERY_PROBE_SUCCESS_THRESHOLD
            && protocol.cooldown_until.is_none_or(|until| now >= until)
            && now.duration_since(protocol.last_target_adjustment) >= RECOVERY_PROBE_INTERVAL
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
            .filter_map(|is_http2| self.protocol_target(is_http2).cooldown_until)
            .map(|until| until.saturating_duration_since(Instant::now()))
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
        assert_eq!(controller.finish_round(false).range_target, Some(15));
        assert_eq!(controller.connection_target(false), 15);
        assert_eq!(controller.connection_target(true), 4);

        controller.record_capacity_failure(true);
        assert_eq!(controller.finish_round(true).range_target, Some(15));
        assert_eq!(controller.range_target(true), 15);
    }

    #[test]
    fn retry_budget_is_preserved_until_single_range() {
        let mut controller = controller();
        assert!(controller.preserve_retry_budget(false));
        assert!(controller.preserve_retry_budget(true));

        for expected in (1..=15).rev() {
            controller.record_capacity_failure(false);
            assert_eq!(controller.finish_round(false).range_target, Some(expected));
        }
        assert_eq!(controller.connection_target(false), 1);
        assert!(!controller.preserve_retry_budget(false));

        for expected in (1..=15).rev() {
            controller.record_capacity_failure(true);
            controller.finish_round(true);
            assert_eq!(controller.range_target(true), expected);
        }
        assert_eq!(controller.connection_target(true), 1);
        assert_eq!(controller.range_target(true), 1);
        assert!(!controller.preserve_retry_budget(true));
    }

    #[test]
    fn capacity_rejection_drops_to_the_served_ranges_and_backs_off() {
        let mut controller = controller();
        let wait = controller.record_capacity_rejection(false, 3, None);
        assert_eq!(controller.connection_target(false), 3);
        assert!(wait >= Duration::from_millis(500) && wait <= Duration::from_millis(1500));
        assert!(
            !controller.can_start(0, false),
            "new ranges wait out the backoff"
        );
        assert!(controller.cooldown_remaining().is_some());

        // The rest of the burst lowers no further than needed and is not
        // another backoff step.
        controller.record_capacity_rejection(false, 3, None);
        assert_eq!(controller.connection_target(false), 3);
        assert_eq!(controller.backoff_steps(), 1);

        assert_eq!(
            controller.record_capacity_rejection(false, 0, Some(Duration::from_secs(7))),
            Duration::from_secs(7),
            "Retry-After is honoured"
        );
        assert_eq!(controller.connection_target(false), 1);
        assert_eq!(controller.connection_target(true), 4);
    }

    #[test]
    fn successes_reset_the_backoff_without_raising_the_reduced_target() {
        let mut controller = HttpAdaptiveConcurrency::new(16, 3, 4, 4, 0);
        controller.record_capacity_rejection(false, 0, Some(Duration::ZERO));
        assert_eq!(controller.connection_target(false), 1);
        assert_eq!(controller.backoff_steps(), 1);

        controller.record_success(false);
        assert_eq!(controller.backoff_steps(), 0);
        for _ in 0..3 {
            controller.record_success(false);
        }
        assert_eq!(controller.connection_target(false), 1);
        assert_eq!(controller.range_target(false), 1);
    }

    #[test]
    fn recovery_probe_waits_for_stability_and_cooldown_then_adds_one_range() {
        let mut controller = controller();
        controller.record_capacity_failure(false);
        assert_eq!(controller.finish_round(false).range_target, Some(15));
        controller.http1.last_target_adjustment =
            Instant::now() - RECOVERY_PROBE_INTERVAL - Duration::from_millis(1);
        controller.http1.cooldown_until = Some(Instant::now() + Duration::from_secs(1));

        for _ in 0..RECOVERY_PROBE_SUCCESS_THRESHOLD {
            controller.record_success(false);
        }
        assert_eq!(controller.finish_round(false), AdaptiveUpdate::default());
        assert_eq!(controller.range_target(false), 15);

        controller.record_failure(false);
        controller.http1.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
        controller.http1.last_target_adjustment =
            Instant::now() - RECOVERY_PROBE_INTERVAL - Duration::from_millis(1);
        for _ in 0..RECOVERY_PROBE_SUCCESS_THRESHOLD {
            controller.record_success(false);
        }

        let update = controller.finish_round(false);
        assert_eq!(update.range_target, Some(16));
        assert!(update.increased);
        assert_eq!(controller.range_target(false), 16);
    }

    #[test]
    fn capacity_backoff_doubles_to_thirty_seconds_with_jitter() {
        for (step, base) in [(1, 1), (2, 2), (3, 4), (5, 16), (6, 30), (20, 30)] {
            let wait = capacity_backoff(step);
            let base = Duration::from_secs(base);
            assert!(
                wait >= base.mul_f64(0.5) && wait <= base.mul_f64(1.5),
                "step {step}: {wait:?}"
            );
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
}
