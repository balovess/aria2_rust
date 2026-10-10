use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::constants::HTTP_SPEED_UPDATE_INTERVAL_MS;
use crate::request::request_group::AtomicProgress;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const CONCURRENT_SPEED_WINDOW: Duration = Duration::from_secs(10);
const RECENT_GOODPUT_SAMPLE_INTERVAL_NANOS: u64 = 500_000_000;
const RECENT_GOODPUT_SAMPLE_BYTES: u64 = 256 * 1024;
const RECENT_GOODPUT_STALE_NANOS: u64 = 2_000_000_000;

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

#[derive(Clone, Copy)]
struct SpeedSample {
    at_nanos: u64,
    bytes: u64,
}

struct ProgressSpeed {
    started_at: Instant,
    last_sample_nanos: AtomicU64,
    sample_bytes: AtomicU64,
    // The transfer hot path aggregates bytes atomically; only the sampler
    // takes this lock to keep the bounded live-speed history.
    samples: Mutex<VecDeque<SpeedSample>>,
    sampling: AtomicBool,
}

struct ProgressSpeedSamplingGuard<'a>(&'a AtomicBool);

impl Drop for ProgressSpeedSamplingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SegmentProgressTracker {
    pub(crate) fn new(initial_completed: u64, progress: Arc<AtomicProgress>) -> Arc<Self> {
        let started_at = Instant::now();
        Arc::new(Self {
            total: AtomicU64::new(initial_completed),
            progress,
            speed: ProgressSpeed {
                started_at,
                last_sample_nanos: AtomicU64::new(0),
                sample_bytes: AtomicU64::new(0),
                samples: Mutex::new(VecDeque::new()),
                sampling: AtomicBool::new(false),
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

    pub(crate) fn refresh_speed(&self) {
        let now_nanos = self.elapsed_nanos();
        let _ = self.speed.refresh_at(&self.progress, now_nanos);
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

    /// Return the recent throughput for this Range request, or zero after its
    /// last network activity has been quiet for two seconds.
    ///
    /// A low value does not make a request stalled. A request is stalled only
    /// when it has received no bytes for the configured inactivity timeout.
    pub(crate) fn recent_throughput_bps(&self) -> u64 {
        let now = self.tracker.elapsed_nanos();
        let last_activity = self.last_activity_nanos.load(Ordering::Acquire);
        fresh_goodput_bps(
            self.recent_throughput_bps.load(Ordering::Acquire),
            now,
            last_activity,
        )
    }

    /// Bytes reported by this Range's progress sampler.
    pub(crate) fn downloaded_bytes(&self) -> u64 {
        self.reported.load(Ordering::Acquire)
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
        let previous_sample_time = self.last_sample_nanos.load(Ordering::Acquire);
        let previous_sample_bytes = self.last_sample_bytes.load(Ordering::Acquire);
        if now.saturating_sub(previous_sample_time) >= RECENT_GOODPUT_SAMPLE_INTERVAL_NANOS
            || downloaded.saturating_sub(previous_sample_bytes) >= RECENT_GOODPUT_SAMPLE_BYTES
        {
            let previous_sample_time = self.last_sample_nanos.swap(now, Ordering::AcqRel);
            let previous_sample_bytes = self.last_sample_bytes.swap(downloaded, Ordering::AcqRel);
            let sample_elapsed = now.saturating_sub(previous_sample_time);
            if sample_elapsed > 0 && downloaded >= previous_sample_bytes {
                let throughput = (downloaded - previous_sample_bytes)
                    .saturating_mul(NANOS_PER_SECOND)
                    / sample_elapsed;
                self.recent_throughput_bps
                    .store(throughput, Ordering::Release);
            }
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

fn fresh_goodput_bps(goodput_bps: u64, now_nanos: u64, last_activity_nanos: u64) -> u64 {
    if now_nanos.saturating_sub(last_activity_nanos) >= RECENT_GOODPUT_STALE_NANOS {
        0
    } else {
        goodput_bps
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
        let _ = self.sample_at(progress, now);
    }

    #[cfg(test)]
    fn record_at(&self, delta: u64, progress: &AtomicProgress, now_nanos: u64) -> Option<u64> {
        self.sample_bytes.fetch_add(delta, Ordering::Relaxed);
        self.sample_at(progress, now_nanos)
    }

    fn sample_at(&self, progress: &AtomicProgress, now_nanos: u64) -> Option<u64> {
        let last_sample_nanos = self.last_sample_nanos.load(Ordering::Acquire);
        let interval_nanos = HTTP_SPEED_UPDATE_INTERVAL_MS.saturating_mul(1_000_000);
        if now_nanos.saturating_sub(last_sample_nanos) < interval_nanos
            || self
                .sampling
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        let _sampling_guard = ProgressSpeedSamplingGuard(&self.sampling);

        let last_sample_nanos = self.last_sample_nanos.load(Ordering::Acquire);
        if now_nanos.saturating_sub(last_sample_nanos) < interval_nanos {
            return None;
        }

        let sample_bytes = self.sample_bytes.swap(0, Ordering::AcqRel);
        let window_nanos = CONCURRENT_SPEED_WINDOW.as_nanos().min(u128::from(u64::MAX)) as u64;
        let sample_elapsed_nanos = now_nanos.saturating_sub(last_sample_nanos);
        // A transfer resuming after an idle period starts a fresh sample
        // window instead of charging its first bytes for the entire pause.
        let sample = if sample_elapsed_nanos >= window_nanos {
            SpeedSample {
                at_nanos: now_nanos.saturating_sub(interval_nanos),
                bytes: sample_bytes,
            }
        } else {
            SpeedSample {
                at_nanos: last_sample_nanos,
                bytes: sample_bytes,
            }
        };
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sample_bytes > 0 {
            samples.push_back(sample);
        }
        while samples
            .front()
            .is_some_and(|sample| now_nanos.saturating_sub(sample.at_nanos) > window_nanos)
        {
            samples.pop_front();
        }
        let speed_bps = speed_from_samples(&samples, now_nanos, window_nanos);
        drop(samples);

        progress.set_download_speed(speed_bps);
        self.last_sample_nanos.store(now_nanos, Ordering::Release);
        Some(speed_bps)
    }

    fn refresh_at(&self, progress: &AtomicProgress, now_nanos: u64) -> Option<u64> {
        self.sampling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let _sampling_guard = ProgressSpeedSamplingGuard(&self.sampling);
        let window_nanos = CONCURRENT_SPEED_WINDOW.as_nanos().min(u128::from(u64::MAX)) as u64;
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while samples
            .front()
            .is_some_and(|sample| now_nanos.saturating_sub(sample.at_nanos) > window_nanos)
        {
            samples.pop_front();
        }
        let speed_bps = speed_from_samples(&samples, now_nanos, window_nanos);
        drop(samples);
        progress.set_download_speed(speed_bps);
        Some(speed_bps)
    }
}

fn speed_from_samples(samples: &VecDeque<SpeedSample>, now_nanos: u64, window_nanos: u64) -> u64 {
    let Some(oldest_sample) = samples.front() else {
        return 0;
    };
    let window_bytes = samples
        .iter()
        .fold(0_u64, |total, sample| total.saturating_add(sample.bytes));
    let elapsed_nanos = now_nanos
        .saturating_sub(oldest_sample.at_nanos)
        .min(window_nanos)
        .max(1);
    ((u128::from(window_bytes) * u128::from(NANOS_PER_SECOND)) / u128::from(elapsed_nanos))
        .min(u128::from(u64::MAX)) as u64
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

        segment.record(256 * 1024);

        assert!(segment.recent_throughput_bps() > 0);
        assert!(!segment.is_stalled(std::time::Duration::from_secs(60)));
        assert_eq!(
            fresh_goodput_bps(128 * 1024, RECENT_GOODPUT_STALE_NANOS, 0),
            0
        );
        assert_eq!(
            fresh_goodput_bps(128 * 1024, RECENT_GOODPUT_STALE_NANOS - 1, 0),
            128 * 1024
        );
    }

    #[test]
    fn concurrent_speed_smooths_large_sample_drops() {
        let started_at = Instant::now();
        let progress = AtomicProgress::new();
        let speed = ProgressSpeed {
            started_at,
            last_sample_nanos: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::new()),
            sampling: AtomicBool::new(false),
        };

        let first = speed
            .record_at(100 * 1024 * 1024, &progress, 500_000_000)
            .expect("the first complete sample should be reported");
        let second = speed
            .record_at(5 * 1024 * 1024, &progress, 1_000_000_000)
            .expect("the second complete sample should be reported");

        assert_eq!(first, 200 * 1024 * 1024);
        assert_eq!(second, 105 * 1024 * 1024);
        assert!(second < first);
        assert_eq!(progress.download_speed(), second);
    }

    #[test]
    fn concurrent_speed_waits_for_a_full_first_interval() {
        let speed = ProgressSpeed {
            started_at: Instant::now(),
            last_sample_nanos: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::new()),
            sampling: AtomicBool::new(false),
        };
        let progress = AtomicProgress::new();

        assert_eq!(speed.record_at(1024, &progress, 499_999_999), None);
        assert_eq!(progress.download_speed(), 0);

        let sample = speed
            .record_at(1024, &progress, 500_000_000)
            .expect("the first sample should be emitted after 500ms");
        assert_eq!(sample, 4096);
        assert_eq!(progress.download_speed(), 4096);
    }

    #[test]
    fn concurrent_speed_expires_bytes_outside_the_ten_second_window() {
        let progress = AtomicProgress::new();
        let speed = ProgressSpeed {
            started_at: Instant::now(),
            last_sample_nanos: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::new()),
            sampling: AtomicBool::new(false),
        };

        let baseline = speed
            .record_at(5 * 1024 * 1024, &progress, 500_000_000)
            .expect("the baseline sample should be reported");
        let spike = speed
            .record_at(100 * 1024 * 1024, &progress, 1_000_000_000)
            .expect("the spike sample should be reported");

        assert_eq!(baseline, 10 * 1024 * 1024);
        assert_eq!(spike, 105 * 1024 * 1024);
        assert_eq!(progress.download_speed(), spike);

        let after_old_samples_expire = speed
            .record_at(5 * 1024 * 1024, &progress, 11_000_000_000)
            .expect("the next rolling sample should be reported");
        assert_eq!(after_old_samples_expire, 10 * 1024 * 1024);
        assert_eq!(progress.download_speed(), after_old_samples_expire);
    }

    #[test]
    fn concurrent_speed_keeps_late_samples_then_expires_idle_speed() {
        let progress = AtomicProgress::new();
        let speed = ProgressSpeed {
            started_at: Instant::now(),
            last_sample_nanos: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::new()),
            sampling: AtomicBool::new(false),
        };

        let speed_bps = speed
            .record_at(1024 * 1024, &progress, 30_000_000_000)
            .expect("a late sample should still be emitted");

        assert_eq!(speed_bps, 2 * 1024 * 1024);
        assert_eq!(progress.download_speed(), speed_bps);

        let expired_speed = speed
            .refresh_at(&progress, 40_000_000_001)
            .expect("the periodic refresh should expire old samples");
        assert_eq!(expired_speed, 0);
        assert_eq!(progress.download_speed(), 0);
    }

    #[test]
    fn concurrent_speed_sampling_has_single_writer() {
        const THREAD_COUNT: usize = 16;

        let speed = Arc::new(ProgressSpeed {
            started_at: Instant::now(),
            last_sample_nanos: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            samples: Mutex::new(VecDeque::new()),
            sampling: AtomicBool::new(false),
        });
        let progress = Arc::new(AtomicProgress::new());
        let barrier = Arc::new(std::sync::Barrier::new(THREAD_COUNT));
        let threads: Vec<_> = (0..THREAD_COUNT)
            .map(|_| {
                let speed = Arc::clone(&speed);
                let progress = Arc::clone(&progress);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    speed.record_at(1024, &progress, 500_000_000)
                })
            })
            .collect();

        let emitted_samples = threads
            .into_iter()
            .filter_map(|thread| thread.join().expect("sampler thread should not panic"))
            .count();

        assert_eq!(emitted_samples, 1);
        assert!(progress.download_speed() > 0);
    }
}
