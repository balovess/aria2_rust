//! Speed statistics for web-seed downloads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Speed statistics for web-seed downloads.
///
/// Tracks download speed separately from peer downloads.
#[derive(Debug, Default)]
pub struct WebSeedStats {
    /// Total bytes downloaded from web seeds
    pub total_bytes: AtomicU64,
    /// Download start time (for speed calculation)
    pub start_time: Option<Instant>,
    /// Bytes downloaded in the current second (for real-time speed)
    pub current_second_bytes: AtomicU64,
    /// Timestamp of the current second window
    pub current_second_start: Option<Instant>,
}

impl WebSeedStats {
    /// Create a new WebSeedStats instance.
    pub fn new() -> Self {
        Self {
            total_bytes: AtomicU64::new(0),
            start_time: Some(Instant::now()),
            current_second_bytes: AtomicU64::new(0),
            current_second_start: Some(Instant::now()),
        }
    }

    /// Record bytes downloaded from a web seed.
    pub fn record_bytes(&self, bytes: u64) {
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.current_second_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get total bytes downloaded from web seeds.
    pub fn total_bytes_downloaded(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Get average download speed in bytes/sec.
    pub fn average_speed(&self) -> u64 {
        if let Some(start) = self.start_time {
            let elapsed = start.elapsed().as_secs();
            return self
                .total_bytes
                .load(Ordering::Relaxed)
                .checked_div(elapsed)
                .unwrap_or(0);
        }
        0
    }

    /// Get current download speed in bytes/sec (real-time).
    pub fn current_speed(&self) -> u64 {
        if let Some(start) = self.current_second_start {
            let elapsed = start.elapsed();
            if elapsed >= Duration::from_secs(1) {
                // Reset the window
                let bytes = self.current_second_bytes.swap(0, Ordering::Relaxed);
                let secs = elapsed.as_secs_f64();
                return (bytes as f64 / secs) as u64;
            }
            // Within the same second, estimate based on current rate
            let bytes = self.current_second_bytes.load(Ordering::Relaxed);
            let secs = elapsed.as_secs_f64();
            if secs > 0.0 {
                return (bytes as f64 / secs) as u64;
            }
        }
        0
    }
}
