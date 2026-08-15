//! Small feedback controller for HTTP segmented downloads.
//!
//! The controller starts at the configured hard limit. A 429/503 response
//! freezes new work for the current round. Every capacity-limited round takes
//! one connection away, which keeps convergence close to the server limit
//! without creating another large connection burst.

use std::time::{Duration, Instant};

/// Result of one HTTP segment attempt as seen by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptiveOutcome {
    Success,
    CapacityLimited,
    OtherFailure,
}

/// Feedback controller for one HTTP source.
#[derive(Debug)]
pub struct HttpAdaptiveConcurrency {
    hard_limit: usize,
    target: usize,
    round_capacity_failures: usize,
    round_results: usize,
    accepting_new_work: bool,
    settled: bool,
    cooldown_until: Option<Instant>,
    cooldown: Duration,
}

impl HttpAdaptiveConcurrency {
    /// Create a controller. The first round uses the hard limit immediately.
    pub fn new(hard_limit: usize, retry_wait_secs: u64) -> Self {
        Self {
            hard_limit: hard_limit.max(1),
            target: hard_limit.max(1),
            round_capacity_failures: 0,
            round_results: 0,
            accepting_new_work: true,
            settled: false,
            cooldown_until: None,
            cooldown: Duration::from_secs(retry_wait_secs.max(1)),
        }
    }

    pub fn target(&self) -> usize {
        self.target
    }

    pub fn hard_limit(&self) -> usize {
        self.hard_limit
    }

    /// Whether another segment may be started right now.
    pub fn can_start(&mut self, active: usize) -> bool {
        if !self.accepting_new_work || active >= self.target {
            return false;
        }
        if let Some(until) = self.cooldown_until {
            if Instant::now() < until {
                return false;
            }
            self.cooldown_until = None;
        }
        true
    }

    /// Record one completed segment attempt.
    pub fn record(&mut self, outcome: AdaptiveOutcome) {
        self.round_results += 1;
        match outcome {
            AdaptiveOutcome::Success => {}
            AdaptiveOutcome::CapacityLimited => {
                self.round_capacity_failures += 1;
                self.accepting_new_work = false;
            }
            AdaptiveOutcome::OtherFailure => {}
        }
    }

    /// Capacity errors can be retried without consuming the ordinary segment
    /// retry budget while the controller is still above one connection.
    pub fn preserve_retry_budget(&self) -> bool {
        self.target > 1
    }

    /// Finish a round after all active requests have drained.
    ///
    /// Returns the new target when a capacity response changed it. The target
    /// takes one step down per capacity-limited round.
    pub fn finish_round(&mut self) -> Option<usize> {
        if self.round_results == 0 {
            return None;
        }

        let changed = if self.round_capacity_failures == 0 {
            self.settled = true;
            None
        } else if self.target > 1 {
            let next = self.target - 1;
            (next != self.target).then_some(next)
        } else {
            self.settled = true;
            None
        };

        if let Some(next) = changed {
            self.target = next;
            self.cooldown_until = Some(Instant::now() + self.cooldown);
        }
        self.round_capacity_failures = 0;
        self.round_results = 0;
        self.accepting_new_work = true;
        changed
    }

    pub fn cooldown_remaining(&self) -> Option<Duration> {
        self.cooldown_until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero())
    }

    pub fn is_settled(&self) -> bool {
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_hard_limit() {
        let controller = HttpAdaptiveConcurrency::new(16, 0);
        assert_eq!(controller.target(), 16);
        assert_eq!(controller.hard_limit(), 16);
    }

    #[test]
    fn partial_capacity_round_decreases_by_one() {
        let mut controller = HttpAdaptiveConcurrency::new(16, 0);
        for _ in 0..11 {
            controller.record(AdaptiveOutcome::Success);
        }
        for _ in 0..5 {
            controller.record(AdaptiveOutcome::CapacityLimited);
        }

        assert_eq!(controller.finish_round(), Some(15));
        assert_eq!(controller.target(), 15);
        assert!(!controller.can_start(15));
    }

    #[test]
    fn all_capacity_failures_decrease_by_one() {
        let mut controller = HttpAdaptiveConcurrency::new(16, 0);
        for _ in 0..16 {
            controller.record(AdaptiveOutcome::CapacityLimited);
        }
        assert_eq!(controller.finish_round(), Some(15));
        assert_eq!(controller.target(), 15);

        for _ in 0..15 {
            controller.record(AdaptiveOutcome::CapacityLimited);
        }
        assert_eq!(controller.finish_round(), Some(14));
        assert_eq!(controller.target(), 14);
    }

    #[test]
    fn successful_round_stops_adjusting() {
        let mut controller = HttpAdaptiveConcurrency::new(16, 0);
        for _ in 0..16 {
            controller.record(AdaptiveOutcome::Success);
        }
        assert_eq!(controller.finish_round(), None);
        assert!(controller.is_settled());
        assert!(controller.can_start(15));
    }
}
