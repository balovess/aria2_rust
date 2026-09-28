use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

const MIN_RANGE_AGE: Duration = Duration::from_secs(2);
const MIN_OBSERVED_BYTES: u64 = 256 * 1024;
const MIN_REFERENCE_GOODPUT_BPS: u64 = 128 * 1024;
const RETRY_OVERHEAD: Duration = Duration::from_secs(1);
const MIN_RECOVERY_SAVINGS: Duration = Duration::from_secs(1);
const REFERENCE_WINDOW: Duration = Duration::from_secs(30);
const MAX_REFERENCE_SAMPLES: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(super) struct SlowRangeObservation {
    pub goodput_bps: u64,
    pub reference_goodput_bps: u64,
    pub downloaded_bytes: u64,
    pub range_length: u64,
    pub estimated_remaining: Duration,
    pub estimated_retry: Duration,
}

/// Tracks successful Range goodput per authority and limits slow-range
/// recovery to one retry per durable piece during a download.
pub(super) struct SlowRangeRecovery {
    successful_goodput: HashMap<String, VecDeque<(Instant, u64)>>,
    recovered_segments: HashSet<u32>,
}

impl SlowRangeRecovery {
    pub(super) fn new() -> Self {
        Self {
            successful_goodput: HashMap::new(),
            recovered_segments: HashSet::new(),
        }
    }

    pub(super) fn record_success(
        &mut self,
        authority: &str,
        downloaded_bytes: u64,
        elapsed: Duration,
    ) {
        if downloaded_bytes < MIN_OBSERVED_BYTES || elapsed.is_zero() {
            return;
        }

        let goodput_bps = bytes_per_second(downloaded_bytes, elapsed);
        let now = Instant::now();
        let samples = self
            .successful_goodput
            .entry(authority.to_owned())
            .or_default();
        samples.retain(|(sampled_at, _)| now.duration_since(*sampled_at) <= REFERENCE_WINDOW);
        if samples.len() == MAX_REFERENCE_SAMPLES {
            samples.pop_front();
        }
        samples.push_back((now, goodput_bps));
    }

    pub(super) fn slow_outlier(
        &self,
        authority: &str,
        segment_index: u32,
        downloaded_bytes: u64,
        recent_goodput_bps: u64,
        range_length: u64,
        elapsed: Duration,
    ) -> Option<SlowRangeObservation> {
        if self.recovered_segments.contains(&segment_index)
            || elapsed < MIN_RANGE_AGE
            || downloaded_bytes < MIN_OBSERVED_BYTES
            || range_length == 0
            || downloaded_bytes >= range_length
        {
            return None;
        }

        let reference_goodput_bps = self.reference_goodput(authority)?;
        if reference_goodput_bps < MIN_REFERENCE_GOODPUT_BPS {
            return None;
        }

        if u128::from(recent_goodput_bps) * 4 >= u128::from(reference_goodput_bps) {
            return None;
        }

        let remaining_bytes = range_length.saturating_sub(downloaded_bytes);
        let estimated_remaining = estimated_duration(remaining_bytes, recent_goodput_bps);
        let estimated_retry =
            estimated_duration(range_length, reference_goodput_bps).saturating_add(RETRY_OVERHEAD);
        if estimated_remaining <= estimated_retry.saturating_add(MIN_RECOVERY_SAVINGS) {
            return None;
        }

        Some(SlowRangeObservation {
            goodput_bps: recent_goodput_bps,
            reference_goodput_bps,
            downloaded_bytes,
            range_length,
            estimated_remaining,
            estimated_retry,
        })
    }

    pub(super) fn mark_recovered(&mut self, segment_index: u32) {
        self.recovered_segments.insert(segment_index);
    }

    fn reference_goodput(&self, authority: &str) -> Option<u64> {
        let now = Instant::now();
        let mut samples = self
            .successful_goodput
            .get(authority)?
            .iter()
            .filter(|(sampled_at, _)| now.duration_since(*sampled_at) <= REFERENCE_WINDOW)
            .map(|(_, goodput_bps)| *goodput_bps)
            .collect::<Vec<_>>();
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        Some(samples[samples.len() / 2])
    }
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> u64 {
    let nanos = elapsed.as_nanos().max(1);
    (u128::from(bytes) * 1_000_000_000 / nanos).min(u128::from(u64::MAX)) as u64
}

fn estimated_duration(bytes: u64, goodput_bps: u64) -> Duration {
    if goodput_bps == 0 {
        return Duration::MAX;
    }

    let nanos = (u128::from(bytes) * 1_000_000_000 / u128::from(goodput_bps))
        .min(u128::from(u64::MAX)) as u64;
    Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_outlier_requires_a_fresh_same_authority_reference_and_retries_once() {
        let mut recovery = SlowRangeRecovery::new();
        recovery.record_success("example.test:443", 1024 * 1024, Duration::from_secs(1));
        let observation = recovery
            .slow_outlier(
                "example.test:443",
                7,
                256 * 1024,
                128 * 1024,
                1024 * 1024,
                Duration::from_secs(3),
            )
            .expect("a four-times-slower partial range should be recovered");
        assert_eq!(observation.downloaded_bytes, 256 * 1024);
        assert_eq!(observation.range_length, 1024 * 1024);

        recovery.mark_recovered(7);
        assert!(
            recovery
                .slow_outlier(
                    "example.test:443",
                    7,
                    256 * 1024,
                    128 * 1024,
                    1024 * 1024,
                    Duration::from_secs(4),
                )
                .is_none()
        );
        assert!(
            recovery
                .slow_outlier(
                    "other.test:443",
                    8,
                    256 * 1024,
                    128 * 1024,
                    1024 * 1024,
                    Duration::from_secs(3),
                )
                .is_none()
        );
    }
}
