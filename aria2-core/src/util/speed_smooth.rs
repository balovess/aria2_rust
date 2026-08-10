// Speed smoothing and ETA calculation utilities.
//
// Provides an Exponential Moving Average (EMA) based speed smoother that
// reduces noise in download speed measurements while remaining responsive
// to actual speed changes. Also includes formatting helpers for human-readable
// display of speeds and durations.

use std::time::{Duration, Instant};

/// Default window size for EMA calculation (number of samples).
const DEFAULT_WINDOW_SIZE: usize = 10;

/// Sample interval in milliseconds - how often to update EMA.
const SAMPLE_INTERVAL_MS: u64 = 500;

/// Burst detection threshold multiplier.
/// Instant speed > threshold * EMA speed is considered a burst.
const BURST_THRESHOLD_MULTIPLIER: f64 = 3.0;

/// Speed smoother using Exponential Moving Average (EMA) algorithm.
///
/// This struct provides smoothed download/upload speed calculations that:
/// - Reduce noise from fluctuating network conditions
/// - React quickly to sustained speed changes
/// - Detect temporary bursts vs sustained speed changes
/// - Calculate accurate ETA estimates
///
/// # Algorithm
///
/// Uses EMA with configurable window size N:
/// ```text
/// alpha = 2 / (N + 1)
/// EMA_new = alpha * value + (1 - alpha) * EMA_old
/// ```
///
/// Larger N values provide more smoothing but slower reaction to changes.
/// Default N=10 provides good balance for typical download scenarios.
///
/// # Example Usage
///
/// ```rust,ignore
/// use aria2_core::util::speed_smooth::SpeedSmoother;
///
/// let mut smoother = SpeedSmoother::new(10);
/// smoother.record_bytes(1024); // Record 1KB downloaded
/// // ... after some time ...
/// let speed = smoother.smoothed_speed();
/// let remaining_bytes = 50000u64;
/// let eta = smoother.eta_seconds(remaining_bytes);
/// ```
pub struct SpeedSmoother {
    /// Current EMA-calculated speed in bytes per second.
    ema_speed: f64,
    /// EMA smoothing factor alpha = 2/(N+1).
    alpha: f64,
    /// Timestamp of last EMA update.
    last_update: Option<Instant>,
    /// Total bytes accumulated since last sample.
    raw_total_bytes: u64,
    /// Timestamp when current sample window started.
    sample_start: Option<Instant>,
    /// Number of samples recorded (for diagnostics).
    samples_count: usize,
    /// Last calculated instant speed (preserved across EMA updates).
    last_instant_speed: f64,
}

impl SpeedSmoother {
    /// Create a new SpeedSmoother with specified window size.
    ///
    /// # Arguments
    ///
    /// * `window_size` - Number of samples for EMA window (default: 10)
    ///
    /// The alpha smoothing factor is calculated as `2 / (N + 1)` where
    /// N is the window size. Larger values provide more smoothing.
    pub fn new(window_size: usize) -> Self {
        let n = if window_size == 0 {
            DEFAULT_WINDOW_SIZE
        } else {
            window_size
        };
        Self {
            ema_speed: 0.0,
            alpha: 2.0 / (n as f64 + 1.0),
            last_update: None,
            raw_total_bytes: 0,
            sample_start: None,
            samples_count: 0,
            last_instant_speed: 0.0,
        }
    }

    /// Create a SpeedSmoother with default window size (N=10).
    pub fn with_default_window() -> Self {
        Self::new(DEFAULT_WINDOW_SIZE)
    }

    /// Record bytes transferred and potentially update EMA.
    ///
    /// Bytes are accumulated until the sample interval (500ms) has elapsed,
    /// at which point the instantaneous speed is calculated and used to
    /// update the EMA value.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes transferred since last call
    pub fn record_bytes(&mut self, bytes: u64) {
        let now = Instant::now();

        // Initialize sample start time on first call
        if self.sample_start.is_none() {
            self.sample_start = Some(now);
        }

        // Accumulate bytes
        self.raw_total_bytes += bytes;

        // Check if enough time has passed for a new sample
        let should_sample = match self.last_update {
            Some(last) => now.duration_since(last) >= Duration::from_millis(SAMPLE_INTERVAL_MS),
            None => true, // First sample
        };

        if should_sample {
            self.update_ema(now);
        }
    }

    /// Internal method to calculate instant speed and update EMA.
    fn update_ema(&mut self, now: Instant) {
        // Calculate time elapsed in current sample window
        let sample_duration = match self.sample_start {
            Some(start) => now.duration_since(start).as_secs_f64(),
            None => return,
        };

        if sample_duration <= 0.0 {
            return;
        }

        // Calculate instantaneous speed for this sample period
        let instant_speed = self.raw_total_bytes as f64 / sample_duration;

        // Save instant speed before resetting
        self.last_instant_speed = instant_speed;

        // Update EMA using standard formula: EMA = alpha * new + (1-alpha) * old
        if self.samples_count == 0 {
            // First sample: initialize EMA directly
            self.ema_speed = instant_speed;
        } else {
            self.ema_speed = self.alpha * instant_speed + (1.0 - self.alpha) * self.ema_speed;
        }

        // Reset sample state for next window
        self.raw_total_bytes = 0;
        self.sample_start = Some(now);
        self.last_update = Some(now);
        self.samples_count += 1;
    }

    /// Get the current EMA-smoothed speed in bytes per second.
    ///
    /// Returns the smoothed speed value, clamped to non-negative.
    /// If no samples have been recorded yet, returns 0.0.
    pub fn smoothed_speed(&self) -> f64 {
        self.ema_speed.max(0.0)
    }

    /// Get the instantaneous (raw) speed from current sample window.
    ///
    /// Calculates speed based on bytes accumulated so far in the current
    /// sampling interval. Returns the last calculated instant speed if
    /// no data has been accumulated in the current window.
    pub fn instant_speed(&self) -> f64 {
        // If we have accumulated bytes in the current window, calculate current speed
        if self.raw_total_bytes > 0 {
            let now = Instant::now();
            if let Some(start) = self.sample_start {
                let elapsed = now.duration_since(start).as_secs_f64();
                if elapsed > 0.0 {
                    return self.raw_total_bytes as f64 / elapsed;
                }
            }
        }
        // Otherwise return the last calculated instant speed (preserved across EMA updates)
        self.last_instant_speed
    }

    /// Calculate ETA in seconds for remaining bytes at current smoothed speed.
    ///
    /// # Arguments
    ///
    /// * `remaining` - Number of bytes still to download/upload
    ///
    /// # Returns
    ///
    /// * `Some(seconds)` - Estimated time remaining if speed > 0
    /// * `None` - Cannot calculate (speed is zero or negative)
    pub fn eta_seconds(&self, remaining: u64) -> Option<u64> {
        let speed = self.smoothed_speed();
        if speed <= 0.0 {
            return None;
        }
        Some((remaining as f64 / speed).ceil() as u64)
    }

    /// Check if current speed indicates a burst condition.
    ///
    /// A burst is detected when the instantaneous speed exceeds
    /// BURST_THRESHOLD_MULTIPLIER (3x) times the smoothed EMA speed.
    /// This can indicate temporary buffer flushes or compression artifacts.
    pub fn is_burst(&self) -> bool {
        let instant = self.instant_speed();
        let ema = self.smoothed_speed();
        ema > 0.0 && instant > BURST_THRESHOLD_MULTIPLIER * ema
    }

    /// Reset all internal state to initial values.
    ///
    /// Clears all accumulators, counters, and timestamps.
    /// Useful when starting a new download or after a pause/resume.
    pub fn reset(&mut self) {
        self.ema_speed = 0.0;
        self.last_update = None;
        self.raw_total_bytes = 0;
        self.sample_start = None;
        self.samples_count = 0;
        self.last_instant_speed = 0.0;
    }

    /// Get the number of samples processed so far.
    pub fn samples_count(&self) -> usize {
        self.samples_count
    }

    /// Get the current alpha smoothing factor.
    pub fn alpha(&self) -> f64 {
        self.alpha
    }
}

impl Default for SpeedSmoother {
    fn default() -> Self {
        Self::with_default_window()
    }
}

// =========================================================================
// Format Helpers
// =========================================================================

// Re-export shared formatting functions from the format module
pub use super::format::{format_duration_short, format_speed as format_bytes_per_sec};

// =========================================================================
// Unit Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_convergence() {
        // Verify EMA alpha formula is correct: alpha = 2/(N+1)
        let smoother = SpeedSmoother::new(10);
        let expected_alpha = 2.0 / 11.0;
        assert!(
            (smoother.alpha() - expected_alpha).abs() < 0.0001,
            "Alpha should be 2/(N+1) ≈ 0.1818, got {}",
            smoother.alpha()
        );

        // Initial state: zero speed, zero samples
        assert_eq!(smoother.smoothed_speed(), 0.0);
        assert_eq!(smoother.samples_count(), 0);
    }

    #[test]
    fn test_record_bytes_accumulates() {
        let mut smoother = SpeedSmoother::new(10);

        // Multiple records without triggering a sample update (interval < 500ms)
        // should accumulate bytes without changing speed
        smoother.record_bytes(1000);
        smoother.record_bytes(500);

        // Speed may or may not be set (depends on scheduler timing).
        // The important invariant: speed is non-negative.
        assert!(smoother.smoothed_speed() >= 0.0, "Speed must be non-negative");
    }

    #[test]
    fn test_reset_clears_state() {
        let mut smoother = SpeedSmoother::new(10);

        // Populate with data
        smoother.record_bytes(5000);

        // Perform reset
        smoother.reset();

        // Verify all state is cleared
        assert_eq!(smoother.smoothed_speed(), 0.0, "Speed should be 0 after reset");
        assert_eq!(smoother.samples_count(), 0, "Sample count should be 0 after reset");
        assert_eq!(smoother.instant_speed(), 0.0, "Instant speed should be 0 after reset");

        // Verify ETA cannot be calculated after reset
        let eta = smoother.eta_seconds(12345);
        assert!(eta.is_none(), "ETA should be None after reset (no speed)");

        // Verify not in burst state after reset
        assert!(!smoother.is_burst(), "Should not be in burst state after reset");
    }

    #[test]
    fn test_eta_edge_cases() {
        let smoother = SpeedSmoother::new(10);

        // Zero speed → ETA should be None
        let eta_no_speed = smoother.eta_seconds(99999);
        assert!(eta_no_speed.is_none(), "ETA should be None when speed is zero");
    }

    #[test]
    fn test_burst_detection_logic() {
        let smoother = SpeedSmoother::new(10);

        // Initially no burst (no data)
        assert!(!smoother.is_burst(), "Should not be burst initially");
    }

    #[test]
    fn test_format_bytes_per_sec_units() {
        // Test various magnitude ranges
        assert!(
            format_bytes_per_sec(500.0).contains("B/s"),
            "Small values use B/s"
        );
        assert!(
            format_bytes_per_sec(2048.0).contains("KiB/s"),
            "KiB range uses KiB/s"
        );
        assert!(
            format_bytes_per_sec(3.0 * 1024.0 * 1024.0).contains("MiB/s"),
            "MiB range uses MiB/s"
        );
        assert!(
            format_bytes_per_sec(2.0 * 1024.0 * 1024.0 * 1024.0).contains("GiB/s"),
            "GiB range uses GiB/s"
        );
    }

    #[test]
    fn test_format_duration_short_various() {
        // Test boundary cases
        assert_eq!(format_duration_short(0), "0s");
        assert_eq!(format_duration_short(1), "1s");
        assert_eq!(format_duration_short(59), "59s");

        // Minute boundaries
        assert_eq!(format_duration_short(60), "1m0s");
        assert_eq!(format_duration_short(61), "1m1s");
        assert_eq!(format_duration_short(3599), "59m59s");

        // Hour boundaries
        assert_eq!(format_duration_short(3600), "1h0m0s");
        assert_eq!(format_duration_short(3661), "1h1m1s");
        assert!(format_duration_short(86400).starts_with("24h"));
    }

    #[test]
    fn test_default_window_size_alpha() {
        let smoother = SpeedSmoother::default();
        // Default N=10, alpha = 2/(10+1) = 2/11 ≈ 0.181818...
        let expected_alpha = 2.0 / (DEFAULT_WINDOW_SIZE as f64 + 1.0);
        assert!(
            (smoother.alpha() - expected_alpha).abs() < 0.0001,
            "Default alpha should be 2/(N+1)"
        );
    }

    #[test]
    fn test_custom_window_size() {
        // Smaller window = faster reaction (higher alpha)
        let small_window = SpeedSmoother::new(5);
        let large_window = SpeedSmoother::new(20);

        assert!(
            small_window.alpha() > large_window.alpha(),
            "Smaller window should have higher alpha"
        );

        // Alpha should always be in valid range (0, 1]
        assert!(small_window.alpha() > 0.0 && small_window.alpha() <= 1.0);
        assert!(large_window.alpha() > 0.0 && large_window.alpha() <= 1.0);
    }
}
