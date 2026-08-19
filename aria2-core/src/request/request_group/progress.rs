use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::ActivitySignal;

// `Instant` cannot be stored directly in an atomic. Keep one process-wide
// monotonic origin and store nanoseconds from that origin instead.
static ACTIVITY_CLOCK_START: OnceLock<Instant> = OnceLock::new();

fn activity_clock_start() -> Instant {
    *ACTIVITY_CLOCK_START.get_or_init(Instant::now)
}

fn current_activity_ticks() -> u64 {
    Instant::now()
        .duration_since(activity_clock_start())
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

fn instant_from_activity_ticks(ticks: u64) -> Instant {
    activity_clock_start()
        .checked_add(Duration::from_nanos(ticks))
        .unwrap_or_else(activity_clock_start)
}

/// Lock-free progress tracking for a download task.
///
/// Extracted from `RequestGroup` so that the hot-path download code can
/// update progress via `Arc<AtomicProgress>` without acquiring the outer
/// `RwLock<RequestGroup>`. All fields are atomic — no locking required.
pub struct AtomicProgress {
    completed_length: AtomicU64,
    total_length: AtomicU64,
    /// Total uploaded bytes (BT only). Mirrors C++ `RequestGroup::getUploadLength()`.
    upload_length: AtomicU64,
    download_speed: AtomicU64,
    upload_speed: AtomicU64,
    last_network_activity: AtomicU64,
    activity_signal: OnceLock<Arc<ActivitySignal>>,
}

impl Default for AtomicProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicProgress {
    pub fn new() -> Self {
        let now = current_activity_ticks();
        Self {
            completed_length: AtomicU64::new(0),
            total_length: AtomicU64::new(0),
            upload_length: AtomicU64::new(0),
            download_speed: AtomicU64::new(0),
            upload_speed: AtomicU64::new(0),
            last_network_activity: AtomicU64::new(now),
            activity_signal: OnceLock::new(),
        }
    }

    pub(crate) fn attach_activity_signal(&self, signal: Arc<ActivitySignal>) {
        let _ = self.activity_signal.set(signal);
    }

    fn notify_activity(&self) {
        if let Some(signal) = self.activity_signal.get() {
            signal.notify();
        }
    }

    /// Reset the I/O inactivity clock for a newly started command attempt.
    pub(crate) fn reset_network_activity(&self) {
        self.last_network_activity
            .store(current_activity_ticks(), Ordering::Release);
    }

    /// Record that non-empty payload bytes were received from the network.
    ///
    /// This is deliberately separate from completed-length updates: bytes can
    /// be buffered, verified, or waiting for a disk write while the connection
    /// is still healthy.
    pub(crate) fn record_network_activity(&self) {
        self.last_network_activity
            .fetch_max(current_activity_ticks(), Ordering::AcqRel);
    }

    pub(crate) fn last_network_activity(&self) -> Instant {
        instant_from_activity_ticks(self.last_network_activity.load(Ordering::Acquire))
    }

    pub fn completed_length(&self) -> u64 {
        self.completed_length.load(Ordering::Relaxed)
    }

    pub fn set_completed_length(&self, v: u64) {
        if self.completed_length.swap(v, Ordering::Relaxed) != v {
            self.notify_activity();
        }
    }

    pub fn total_length(&self) -> u64 {
        self.total_length.load(Ordering::Relaxed)
    }

    pub fn set_total_length(&self, v: u64) {
        if self.total_length.swap(v, Ordering::Relaxed) != v {
            self.notify_activity();
        }
    }

    pub fn download_speed(&self) -> u64 {
        self.download_speed.load(Ordering::Relaxed)
    }

    pub fn set_download_speed(&self, v: u64) {
        if self.download_speed.swap(v, Ordering::Relaxed) != v {
            self.notify_activity();
        }
    }

    pub fn upload_speed(&self) -> u64 {
        self.upload_speed.load(Ordering::Relaxed)
    }

    pub fn set_upload_speed(&self, v: u64) {
        if self.upload_speed.swap(v, Ordering::Relaxed) != v {
            self.notify_activity();
        }
    }

    pub fn upload_length(&self) -> u64 {
        self.upload_length.load(Ordering::Relaxed)
    }

    pub fn set_upload_length(&self, v: u64) {
        if self.upload_length.swap(v, Ordering::Relaxed) != v {
            self.notify_activity();
        }
    }
}
