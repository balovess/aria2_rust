use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::constants::HTTP_SPEED_UPDATE_INTERVAL_MS;
use crate::request::request_group::AtomicProgress;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Aggregates progress from all range requests without a channel or task per
/// segment. Each segment owns one handle and contributes only its delta.
pub(crate) struct SegmentProgressTracker {
    total: AtomicU64,
    progress: Arc<AtomicProgress>,
    speed: ProgressSpeed,
    segment_count: AtomicU64,
    update_count: AtomicU64,
    rollback_count: AtomicU64,
}

/// Progress state owned by one HTTP range request.
pub(crate) struct SegmentProgress {
    tracker: Arc<SegmentProgressTracker>,
    reported: AtomicU64,
    last_activity_nanos: AtomicU64,
    last_sample_nanos: AtomicU64,
    last_sample_bytes: AtomicU64,
    recent_throughput_bps: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentProgressStats {
    pub segments: u64,
    pub updates: u64,
    pub rollbacks: u64,
}

struct ProgressSpeed {
    started_at: Instant,
    last_sample_nanos: AtomicU64,
    sample_bytes: AtomicU64,
}

impl SegmentProgressTracker {
    pub(crate) fn new(initial_completed: u64, progress: Arc<AtomicProgress>) -> Arc<Self> {
        Arc::new(Self {
            total: AtomicU64::new(initial_completed),
            progress,
            speed: ProgressSpeed {
                started_at: Instant::now(),
                last_sample_nanos: AtomicU64::new(0),
                sample_bytes: AtomicU64::new(0),
            },
            segment_count: AtomicU64::new(0),
            update_count: AtomicU64::new(0),
            rollback_count: AtomicU64::new(0),
        })
    }

    pub(crate) fn new_segment(self: &Arc<Self>) -> Arc<SegmentProgress> {
        self.segment_count.fetch_add(1, Ordering::Relaxed);
        Arc::new(SegmentProgress {
            tracker: Arc::clone(self),
            reported: AtomicU64::new(0),
            last_activity_nanos: AtomicU64::new(self.elapsed_nanos()),
            last_sample_nanos: AtomicU64::new(self.elapsed_nanos()),
            last_sample_bytes: AtomicU64::new(0),
            recent_throughput_bps: AtomicU64::new(0),
        })
    }

    pub(crate) fn total(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }

    pub(crate) fn stats(&self) -> SegmentProgressStats {
        SegmentProgressStats {
            segments: self.segment_count.load(Ordering::Relaxed),
            updates: self.update_count.load(Ordering::Relaxed),
            rollbacks: self.rollback_count.load(Ordering::Relaxed),
        }
    }
}

impl SegmentProgress {
    /// Refresh the group-level I/O inactivity clock for a received range
    /// chunk. This is independent of the coarser display progress threshold.
    pub(crate) fn record_network_activity(&self) {
        self.last_activity_nanos
            .store(self.tracker.elapsed_nanos(), Ordering::Release);
        self.tracker.progress.record_network_activity();
    }

    /// Check this Range independently from the group-level activity clock.
    pub(crate) fn is_stalled(&self, timeout: std::time::Duration) -> bool {
        let now = self.tracker.elapsed_nanos();
        let last = self.last_activity_nanos.load(Ordering::Acquire);
        now.saturating_sub(last) >= timeout.as_nanos().min(u128::from(u64::MAX)) as u64
    }

    /// Return the most recently measured throughput for this Range request.
    ///
    /// A low value does not make a request stalled. A request is stalled only
    /// when it has received no bytes for the configured inactivity timeout.
    pub(crate) fn recent_throughput_bps(&self) -> u64 {
        self.recent_throughput_bps.load(Ordering::Acquire)
    }

    /// Record a monotonic byte count relative to this segment's range.
    pub(crate) fn record(&self, downloaded: u64) {
        let previous = loop {
            let previous = self.reported.load(Ordering::Acquire);
            if downloaded <= previous {
                return;
            }
            if self
                .reported
                .compare_exchange_weak(previous, downloaded, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break previous;
            }
        };

        let delta = downloaded - previous;
        let now = self.tracker.elapsed_nanos();
        let previous_sample_time = self.last_sample_nanos.swap(now, Ordering::AcqRel);
        let previous_sample_bytes = self.last_sample_bytes.swap(downloaded, Ordering::AcqRel);
        let sample_elapsed = now.saturating_sub(previous_sample_time);
        if sample_elapsed > 0 && downloaded >= previous_sample_bytes {
            let throughput = (downloaded - previous_sample_bytes).saturating_mul(NANOS_PER_SECOND)
                / sample_elapsed;
            self.recent_throughput_bps
                .store(throughput, Ordering::Release);
        }
        let total = self.tracker.total.fetch_add(delta, Ordering::AcqRel) + delta;
        self.tracker.update_count.fetch_add(1, Ordering::Relaxed);
        self.tracker.progress.set_completed_length(total);
        self.tracker.speed.record(delta, &self.tracker.progress);
    }

    /// Remove transient bytes when a segment attempt fails and is retried.
    pub(crate) fn rollback(&self) {
        let reported = self.reported.swap(0, Ordering::AcqRel);
        if reported == 0 {
            return;
        }

        let total = self.tracker.total.fetch_sub(reported, Ordering::AcqRel) - reported;
        self.tracker.rollback_count.fetch_add(1, Ordering::Relaxed);
        self.tracker.progress.set_completed_length(total);
    }
}

impl ProgressSpeed {
    fn record(&self, delta: u64, progress: &AtomicProgress) {
        self.sample_bytes.fetch_add(delta, Ordering::Relaxed);
        let now = self
            .started_at
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        let last = self.last_sample_nanos.load(Ordering::Acquire);
        let interval = HTTP_SPEED_UPDATE_INTERVAL_MS.saturating_mul(1_000_000);
        if now.saturating_sub(last) < interval
            || self
                .last_sample_nanos
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }

        let bytes = self.sample_bytes.swap(0, Ordering::AcqRel);
        let elapsed = now.saturating_sub(last).max(1);
        let speed = bytes.saturating_mul(NANOS_PER_SECOND) / elapsed;
        if speed > 0 {
            progress.set_download_speed(speed);
        }
    }
}

impl SegmentProgressTracker {
    fn elapsed_nanos(&self) -> u64 {
        self.speed
            .started_at
            .elapsed()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_segment_deltas_and_rolls_back_transient_bytes() {
        let progress = Arc::new(AtomicProgress::new());
        let tracker = SegmentProgressTracker::new(100, Arc::clone(&progress));
        let first = tracker.new_segment();
        let second = tracker.new_segment();

        first.record(40);
        second.record(25);
        assert_eq!(tracker.total(), 165);
        assert_eq!(progress.completed_length(), 165);

        first.rollback();
        assert_eq!(tracker.total(), 125);
        assert_eq!(progress.completed_length(), 125);
        assert_eq!(tracker.stats().updates, 2);
        assert_eq!(tracker.stats().rollbacks, 1);
    }

    #[test]
    fn stale_segment_updates_do_not_decrease_progress() {
        let progress = Arc::new(AtomicProgress::new());
        let tracker = SegmentProgressTracker::new(0, Arc::clone(&progress));
        let segment = tracker.new_segment();

        segment.record(100);
        segment.record(80);

        assert_eq!(tracker.total(), 100);
        assert_eq!(tracker.stats().updates, 1);
    }

    #[test]
    fn recent_throughput_does_not_change_inactivity_stall_semantics() {
        let progress = Arc::new(AtomicProgress::new());
        let tracker = SegmentProgressTracker::new(0, progress);
        let segment = tracker.new_segment();

        segment.record(1024);

        assert!(segment.recent_throughput_bps() > 0);
        assert!(!segment.is_stalled(std::time::Duration::from_secs(60)));
    }
}
