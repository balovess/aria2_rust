use std::sync::atomic::{AtomicU64, Ordering};

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
}

impl AtomicProgress {
    pub fn new() -> Self {
        Self {
            completed_length: AtomicU64::new(0),
            total_length: AtomicU64::new(0),
            upload_length: AtomicU64::new(0),
            download_speed: AtomicU64::new(0),
            upload_speed: AtomicU64::new(0),
        }
    }

    pub fn completed_length(&self) -> u64 {
        self.completed_length.load(Ordering::Relaxed)
    }

    pub fn set_completed_length(&self, v: u64) {
        self.completed_length.store(v, Ordering::Relaxed);
    }

    pub fn total_length(&self) -> u64 {
        self.total_length.load(Ordering::Relaxed)
    }

    pub fn set_total_length(&self, v: u64) {
        self.total_length.store(v, Ordering::Relaxed);
    }

    pub fn download_speed(&self) -> u64 {
        self.download_speed.load(Ordering::Relaxed)
    }

    pub fn set_download_speed(&self, v: u64) {
        self.download_speed.store(v, Ordering::Relaxed);
    }

    pub fn upload_speed(&self) -> u64 {
        self.upload_speed.load(Ordering::Relaxed)
    }

    pub fn set_upload_speed(&self, v: u64) {
        self.upload_speed.store(v, Ordering::Relaxed);
    }

    pub fn upload_length(&self) -> u64 {
        self.upload_length.load(Ordering::Relaxed)
    }

    pub fn set_upload_length(&self, v: u64) {
        self.upload_length.store(v, Ordering::Relaxed);
    }
}
