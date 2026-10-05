//! Capacity-only concurrency control for HTTP segmented downloads.
//!
//! Each protocol starts at its configured ceiling. Successful requests never
//! change that target. An explicit server-capacity rejection stops new
//! admissions until the active round drains, then lowers the effective target
//! by one and retries the rejected range. The configured connection/session
//! limits remain hard ceilings and are never rewritten.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AdaptiveUpdate {
    pub connection_target: Option<usize>,
}

#[derive(Debug)]
struct ProtocolTarget {
    target: usize,
    capacity_rejected: bool,
    accepting_new_work: bool,
    cooldown_until: Option<Instant>,
}

impl ProtocolTarget {
    fn new(target: usize) -> Self {
        Self {
            target: target.max(1),
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
