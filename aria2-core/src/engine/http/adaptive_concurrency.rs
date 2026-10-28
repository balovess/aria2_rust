//! Capacity-only concurrency control for HTTP segmented downloads.
//!
//! Each protocol starts at its configured ceiling. Successful requests never
//! change that target. An explicit server-capacity rejection stops new
//! admissions until the active round drains, then lowers the effective target
//! by one and retries the rejected range. The configured connection/session
//! limits remain hard ceilings and are never rewritten.
//!
//! A single-source download instead treats a 429/503 as a rate limit: the
//! target drops at once to the ranges the server is still serving, new
//! ranges pause for Retry-After (or an exponential backoff), and a run of
//! successes raises the target again, up to the ceiling.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AdaptiveUpdate {
    pub connection_target: Option<usize>,
}

#[derive(Debug)]
struct ProtocolTarget {
    target: usize,
    /// The configured limit; `target` never exceeds it.
    ceiling: usize,
    /// Successful ranges since the target last changed.
    successes: usize,
    capacity_rejected: bool,
    accepting_new_work: bool,
    cooldown_until: Option<Instant>,
}

impl ProtocolTarget {
    fn new(target: usize) -> Self {
        Self {
            target: target.max(1),
            ceiling: target.max(1),
            successes: 0,
            capacity_rejected: false,
            accepting_new_work: true,
            cooldown_until: None,
        }
    }
}

/// Capacity controller for one HTTP source. HTTP/1.1 targets physical
/// connections; HTTP/2 targets sessions, with a fixed stream allowance per
/// session.
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
    base.mul_f64(rand::Rng::gen_range(&mut rand::thread_rng(), 0.5..1.5))
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
            http1: ProtocolTarget::new(http1_connection_limit),
            http2: ProtocolTarget::new(http2_session_limit),
            http2_streams_per_session: http2_streams_per_session.max(1),
            retry_wait: Duration::from_secs(retry_wait_secs),
            backoff_steps: 0,
        }
    }

    pub(crate) fn connection_target(&self, is_http2: bool) -> usize {
        self.protocol_target(is_http2).target
    }

    pub(crate) fn range_target(&self, is_http2: bool) -> usize {
        let target = if is_http2 {
            self.connection_target(true)
                .saturating_mul(self.http2_streams_per_session)
        } else {
            self.connection_target(false)
        };
        target.min(self.split_budget).max(1)
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
        protocol.successes = 0;
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

    /// A range succeeded: the backoff starts over, and after a run of
    /// successes the target goes back up by one (a rate limit may have
    /// passed), never above the configured ceiling.
    pub(crate) fn record_success(&mut self, is_http2: bool) {
        self.backoff_steps = 0;
        let protocol = self.protocol_target_mut(is_http2);
        protocol.successes = protocol.successes.saturating_add(1);
        if protocol.target < protocol.ceiling
            && protocol.successes >= protocol.target.saturating_mul(4)
        {
            protocol.target += 1;
            protocol.successes = 0;
        }
    }

    /// Backoff episodes since the last successful range.
    pub(crate) fn backoff_steps(&self) -> u32 {
        self.backoff_steps
    }

    /// Capacity rejections can be retried without consuming ordinary retry
    /// budget while there is still a lower configured target to try.
    pub(crate) fn preserve_retry_budget(&self, is_http2: bool) -> bool {
        self.connection_target(is_http2) > 1
    }

    /// Close the current round after all requests have drained. A successful
    /// round is stable at its current target; only capacity rejection lowers it.
    pub(crate) fn finish_round(&mut self, is_http2: bool) -> AdaptiveUpdate {
        let retry_wait = self.retry_wait;
        let protocol = self.protocol_target_mut(is_http2);
        let changed = if protocol.capacity_rejected && protocol.target > 1 {
            protocol.target -= 1;
            if !retry_wait.is_zero() {
                protocol.cooldown_until = Some(Instant::now() + retry_wait);
            }
            Some(protocol.target)
        } else {
            None
        };
        protocol.capacity_rejected = false;
        protocol.accepting_new_work = true;
        AdaptiveUpdate {
            connection_target: changed,
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
    fn starts_at_each_configured_ceiling_and_success_keeps_it() {
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
        assert_eq!(controller.finish_round(false).connection_target, Some(15));
        assert_eq!(controller.connection_target(false), 15);
        assert_eq!(controller.connection_target(true), 4);

        controller.record_capacity_failure(true);
        assert_eq!(controller.finish_round(true).connection_target, Some(3));
        assert_eq!(controller.range_target(true), 12);
    }

    #[test]
    fn retry_budget_is_preserved_until_single_connection_or_session() {
        let mut controller = controller();
        assert!(controller.preserve_retry_budget(false));
        assert!(controller.preserve_retry_budget(true));

        for expected in (1..=15).rev() {
            controller.record_capacity_failure(false);
            assert_eq!(
                controller.finish_round(false).connection_target,
                Some(expected)
            );
        }
        assert_eq!(controller.connection_target(false), 1);
        assert!(!controller.preserve_retry_budget(false));

        for expected in [3, 2, 1] {
            controller.record_capacity_failure(true);
            assert_eq!(
                controller.finish_round(true).connection_target,
                Some(expected)
            );
        }
        assert_eq!(controller.connection_target(true), 1);
        assert_eq!(controller.range_target(true), 4);
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
    fn successes_reset_the_backoff_and_raise_the_target_to_the_ceiling() {
        let mut controller = HttpAdaptiveConcurrency::new(16, 3, 4, 4, 0);
        controller.record_capacity_rejection(false, 0, Some(Duration::ZERO));
        assert_eq!(controller.connection_target(false), 1);
        assert_eq!(controller.backoff_steps(), 1);

        controller.record_success(false);
        assert_eq!(controller.backoff_steps(), 0);
        for _ in 0..3 {
            controller.record_success(false);
        }
        assert_eq!(
            controller.connection_target(false),
            2,
            "4 x target successes"
        );
        for _ in 0..8 {
            controller.record_success(false);
        }
        assert_eq!(controller.connection_target(false), 3);
        for _ in 0..24 {
            controller.record_success(false);
        }
        assert_eq!(
            controller.connection_target(false),
            3,
            "never above the ceiling"
        );
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
        assert_eq!(controller.connection_target(false), 16);
        assert_eq!(controller.connection_target(true), 4);
    }
}
