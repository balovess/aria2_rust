//! Capacity-only concurrency control for HTTP segmented downloads.
//!
//! Each protocol starts at the highest Range concurrency allowed by its
//! configured limits. An explicit server-capacity rejection stops new
//! admissions until the active round drains, then lowers the target and retries
//! the rejected range. After a cooldown and a stable window of successful
//! requests, the controller probes upward by one Range. Configured connection,
//! session, stream, and split limits remain hard ceilings and are never rewritten.
//!
//! Capacity feedback lowers the Range target multiplicatively and applies a
//! cooldown after explicit server rejection. Healthy Range starts are never
//! paced by a timer.
//! Temporary no-progress retries remain local to a Range and do not alter the
//! global target.

use crate::request::request_group::HttpAdaptiveOptions;
use rand::RngExt;
use std::time::{Duration, Instant};

const RECOVERY_SUCCESS_THRESHOLD: usize = 8;
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

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
    drain_after_capacity_rejection: bool,
    cooldown_until: Option<Instant>,
    successful_ranges_since_adjustment: usize,
}

impl ProtocolTarget {
    fn new(ceiling: usize) -> Self {
        Self {
            ceiling: ceiling.max(1),
            target: ceiling.max(1),
            capacity_rejected: false,
            accepting_new_work: true,
            drain_after_capacity_rejection: false,
            cooldown_until: None,
            successful_ranges_since_adjustment: 0,
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
            HttpAdaptiveOptions {
                enabled: true,
                ..HttpAdaptiveOptions::default()
            },
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
        Self {
            settings,
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
        if !protocol.accepting_new_work
            || (protocol.drain_after_capacity_rejection && active > 0)
            || active >= range_target
        {
            return false;
        }
        if let Some(until) = protocol.cooldown_until {
            if now < until {
                return false;
            }
            protocol.cooldown_until = None;
        }
        true
    }

    /// Freeze new admissions after an explicit server capacity rejection.
    ///
    /// Adaptive mode applies exponential capacity cooldown. Disabled mode
    /// keeps the configured target and only uses ordinary retry timing.
    pub(crate) fn record_capacity_failure(
        &mut self,
        is_http2: bool,
        retry_after: Option<Duration>,
    ) {
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
            if self
                .last_capacity_rejection
                .is_some_and(|last| now.duration_since(last) >= BACKOFF_RESET_AFTER)
            {
                self.backoff_steps = 0;
            }
            self.last_capacity_rejection = Some(now);
            let wait = if settings.enabled {
                self.backoff_steps = self.backoff_steps.saturating_add(1);
                self.retry_wait
                    .max(capacity_backoff_with(self.backoff_steps, &settings))
            } else {
                retry_after.unwrap_or(self.retry_wait)
            };
            if !wait.is_zero() {
                let until = now.checked_add(wait).unwrap_or(now);
                let protocol = self.protocol_target_mut(is_http2);
                protocol.cooldown_until =
                    Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
            }
        }
    }

    /// With adaptive mode enabled, a 429/503 lowers concurrency by the
    /// configured percentage per rejection episode. With it disabled, the
    /// target stays fixed and only explicit Retry-After applies.
    /// Returns the required wait.
    pub(crate) fn record_capacity_rejection(
        &mut self,
        is_http2: bool,
        retry_after: Option<Duration>,
    ) -> Duration {
        let settings = self.settings;
        let now = Instant::now();
        if self
            .last_capacity_rejection
            .is_some_and(|last| now.duration_since(last) >= BACKOFF_RESET_AFTER)
        {
            self.backoff_steps = 0;
        }
        self.last_capacity_rejection = Some(now);
        let protocol = self.protocol_target_mut(is_http2);
        let in_cooldown = protocol.cooldown_until.is_some_and(|until| now < until);
        if !in_cooldown && settings.enabled {
            protocol.target = Self::reduce_target(protocol.target, settings.reduction_percent);
        }
        let protocol = self.protocol_target_mut(is_http2);
        protocol.drain_after_capacity_rejection = true;
        if settings.enabled {
            protocol.accepting_new_work = false;
        }
        protocol.successful_ranges_since_adjustment = 0;
        // One backoff step per episode: the other ranges of the same burst
        // are rejected inside the cooldown it started.
        if !in_cooldown && settings.enabled {
            self.backoff_steps = self.backoff_steps.saturating_add(1);
        }
        let wait = if settings.enabled {
            retry_after
                .unwrap_or_default()
                .max(capacity_backoff_with(self.backoff_steps, &self.settings))
        } else {
            retry_after.unwrap_or_default()
        };
        if !wait.is_zero() {
            let until = now.checked_add(wait).unwrap_or(now);
            let protocol = self.protocol_target_mut(is_http2);
            protocol.cooldown_until =
                Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
        }
        wait
    }

    /// Record a successful range for concurrency recovery. A single successful
    /// range does not clear capacity backoff history.
    pub(crate) fn record_success(&mut self, is_http2: bool) {
        let settings = self.settings;
        let protocol = self.protocol_target_mut(is_http2);
        if !settings.enabled {
            protocol.successful_ranges_since_adjustment = 0;
            return;
        }
        protocol.successful_ranges_since_adjustment = protocol
            .successful_ranges_since_adjustment
            .saturating_add(1);
    }

    /// Reset the recovery window after any failed Range request.
    pub(crate) fn record_failure(&mut self, is_http2: bool) {
        self.protocol_target_mut(is_http2)
            .successful_ranges_since_adjustment = 0;
    }

    /// Backoff episodes since the last sustained 429-free interval.
    pub(crate) fn backoff_steps(&self) -> u32 {
        self.backoff_steps
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Close the current round after all requests have drained. When adaptive
    /// mode is enabled, capacity rejection lowers the Range target and returns
    /// the new target.
    pub(crate) fn finish_round(&mut self, is_http2: bool) -> AdaptiveUpdate {
        let retry_wait = self.retry_wait;
        let settings = self.settings;
        let protocol = self.protocol_target_mut(is_http2);
        let now = Instant::now();
        let (changed, increased) = if protocol.capacity_rejected {
            let changed = if settings.enabled {
                let reduced_target =
                    Self::reduce_target(protocol.target, settings.reduction_percent);
                if reduced_target < protocol.target {
                    protocol.target = reduced_target;
                    Some(protocol.target)
                } else {
                    None
                }
            } else {
                None
            };
            protocol.successful_ranges_since_adjustment = 0;
            if !retry_wait.is_zero() {
                let until = now + retry_wait;
                protocol.cooldown_until =
                    Some(protocol.cooldown_until.map_or(until, |old| old.max(until)));
            }
            (changed, false)
        } else if settings.enabled
            && protocol.target < protocol.ceiling
            && protocol.successful_ranges_since_adjustment >= RECOVERY_SUCCESS_THRESHOLD
            && protocol.cooldown_until.is_none_or(|until| now >= until)
        {
            protocol.target += 1;
            protocol.successful_ranges_since_adjustment = 0;
            (Some(protocol.target), true)
        } else {
            (None, false)
        };
        protocol.capacity_rejected = false;
        protocol.accepting_new_work = true;
        protocol.drain_after_capacity_rejection = false;
        AdaptiveUpdate {
            range_target: changed,
            increased,
        }
    }

    pub(crate) fn cooldown_remaining(&self) -> Option<Duration> {
        [false, true]
            .into_iter()
            .filter_map(|is_http2| self.cooldown_remaining_for(is_http2))
            .max()
    }

    pub(crate) fn cooldown_remaining_for(&self, is_http2: bool) -> Option<Duration> {
        [self
            .protocol_target(is_http2)
            .cooldown_until
            .map(|until| until.saturating_duration_since(Instant::now()))]
        .into_iter()
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
mod tests;
