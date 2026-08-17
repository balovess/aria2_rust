use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use super::ActivitySignal;

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
    activity_signal: OnceLock<Arc<ActivitySignal>>,
}

impl Default for AtomicProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicProgress {
    pub fn new() -> Self {
        Self {
            completed_length: AtomicU64::new(0),
            total_length: AtomicU64::new(0),
            upload_length: AtomicU64::new(0),
            download_speed: AtomicU64::new(0),
            upload_speed: AtomicU64::new(0),
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
