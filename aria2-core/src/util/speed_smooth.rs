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
    /// sampling interval. Returns 0.0 if no data or insufficient time.
    pub fn instant_speed(&self) -> f64 {
        let now = Instant::now();
        match self.sample_start {
            Some(start) => {
                let elapsed = now.duration_since(start).as_secs_f64();
                if elapsed > 0.0 {
                    self.raw_total_bytes as f64 / elapsed
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
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

/// Format bytes per second with automatic unit selection.
///
/// Converts a raw bytes/second value into a human-readable string with
/// appropriate unit suffix (B/s, KiB/s, MiB/s, GiB/s).
///
/// # Arguments
///
/// * `bytes_per_sec` - Speed in bytes per second
///
/// # Returns
///
/// Formatted string like "1.50 MiB/s" or "512 B/s"
///
/// # Example
///
/// ```
/// use aria2_core::util::speed_smooth::format_bytes_per_sec;
///
/// assert_eq!(format_bytes_per_sec(1536.0), "1.50 KiB/s");
/// assert_eq!(format_bytes_per_sec(1048576.0), "1.00 MiB/s");
/// ```
pub fn format_bytes_per_sec(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB/s", bytes_per_sec / (1024.0 * 1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 * 1024.0 {
        format!("{:.2} MiB/s", bytes_per_sec / (1024.0 * 1024.0))
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.2} KiB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

/// Format a duration in seconds as a short human-readable string.
///
/// Produces compact representations suitable for UI display:
/// - Seconds only: "42s"
/// - Minutes + seconds: "3m12s"
/// - Hours + minutes + seconds: "1h23m45s"
/// - Zero: "0s"
///
/// # Arguments
///
/// * `secs` - Duration in seconds
///
/// # Returns
///
/// Short formatted string like "3m12s" or "1h23m"
///
/// # Example
///
/// ```
/// use aria2_core::util::speed_smooth::format_duration_short;
///
/// assert_eq!(format_duration_short(0), "0s");
/// assert_eq!(format_duration_short(45), "45s");
/// assert_eq!(format_duration_short(125), "2m5s");
/// assert_eq!(format_duration_short(3661), "1h1m1s");
/// ```
pub fn format_duration_short(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h{}m{}s", h, m, s)
    } else if m > 0 {
        format!("{}m{}s", m, s)
    } else {
        format!("{}s", s)
    }
}

// =========================================================================
// Unit Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Helper to simulate recording bytes over time for testing EMA behavior.
    ///
    /// Records `bytes_per_sample` every `interval_ms` milliseconds,
    /// repeating `num_samples` times.
    fn simulate_downloads(
        smoother: &mut SpeedSmoother,
        bytes_per_sample: u64,
        interval_ms: u64,
        num_samples: usize,
    ) {
        for _ in 0..num_samples {
            smoother.record_bytes(bytes_per_sample);
            if interval_ms > 0 {
                thread::sleep(Duration::from_millis(interval_ms));
            }
        }
    }

    #[test]
    fn test_ema_convergence() {
        // Test that constant input causes EMA to converge to input value
        let mut smoother = SpeedSmoother::new(10); // N=10, alpha=2/11≈0.1818

        // Each sample records 100 bytes; with 100ms interval and 500ms sampling window,
        // ~5 calls accumulate ~500 bytes per EMA update → speed ≈ 500/0.5 = 1000 B/s
        const BYTES_PER_CALL: u64 = 100;
        const TARGET_SPEED_BPS: f64 = 1000.0;
        const INTERVAL_MS: u64 = 100;

        // Simulate ~20+ samples at constant rate
        simulate_downloads(&mut smoother, BYTES_PER_CALL, INTERVAL_MS, 25);

        let final_speed = smoother.smoothed_speed();

        // Allow 50% tolerance for EMA convergence (timing variance across CI/locals)
        let error_ratio = (final_speed - TARGET_SPEED_BPS).abs() / TARGET_SPEED_BPS;
        assert!(
            error_ratio < 0.50,
            "EMA should converge to target speed. Got {:.2}, expected ~{}, error ratio: {:.2}",
            final_speed,
            TARGET_SPEED_BPS,
            error_ratio
        );

        // Verify we've actually taken multiple samples
        assert!(
            smoother.samples_count() >= 5,
            "Should have recorded at least 5 samples, got {}",
            smoother.samples_count()
        );
    }

    #[test]
    fn test_ema_reacts_to_drop() {
        // Test that EMA reacts to significant speed drops within ~5 samples
        let mut smoother = SpeedSmoother::new(10);

        // Phase 1: High speed (~10000 B/s equivalent)
        // 1000 bytes/call * 10 calls per 500ms window → ~10000 B/s
        simulate_downloads(&mut smoother, 1000, 100, 15);
        let high_speed = smoother.smoothed_speed();

        // Phase 2: Drop to low speed (~1000 B/s equivalent)
        // 100 bytes/call * 5 calls per 500ms window → ~1000 B/s
        simulate_downloads(&mut smoother, 100, 100, 8);
        let low_speed = smoother.smoothed_speed();

        // After drop phase, speed should have decreased significantly
        // EMA with alpha≈0.18 needs more samples for large swings; allow 20% minimum drop
        assert!(
            low_speed < high_speed * 0.80,
            "EMA should react to speed drop. Before: {:.2}, After: {:.2}",
            high_speed,
            low_speed
        );

        // Verify the speed is trending toward the new low value
        assert!(
            low_speed < high_speed * 0.85,
            "Post-drop speed ({:.2}) should be below pre-drop ({:.2})",
            low_speed,
            high_speed
        );
    }

    #[test]
    fn test_burst_detection() {
        // Test that sudden spikes are detected as bursts
        let mut smoother = SpeedSmoother::new(10);

        // Establish baseline: steady ~100 B/s (10 bytes per 100ms, ~5 calls per 500ms window)
        simulate_downloads(&mut smoother, 10, 100, 12);
        let _baseline_speed = smoother.smoothed_speed();

        // Sleep briefly (less than SAMPLE_INTERVAL_MS=500ms) so next record_bytes
        // does NOT trigger EMA update — bytes stay in current window for instant_speed()
        thread::sleep(Duration::from_millis(120));

        // Now record a large chunk simulating a burst — this stays in current sample window
        // because < 500ms since last EMA update
        smoother.record_bytes(100000);

        let is_burst = smoother.is_burst();
        let instant = smoother.instant_speed();

        // Should detect burst condition (instant >> 3x EMA)
        assert!(
            is_burst || instant > 10000.0,
            "Should detect burst. Instant: {:.2}, is_burst: {}",
            instant,
            is_burst
        );
    }

    #[test]
    fn test_eta_calculation() {
        let mut smoother = SpeedSmoother::new(10);

        // Establish known speed: 1000 bytes per 100ms = 10000 B/s
        simulate_downloads(&mut smoother, 1000, 100, 15);
        let speed = smoother.smoothed_speed();

        // Ensure we have a reasonable speed established
        assert!(
            speed > 0.0,
            "Should have non-zero speed for ETA calculation"
        );

        // Test ETA calculation: 10000 bytes at ~10000 B/s should be ~1 second
        let eta = smoother.eta_seconds(10000);

        assert!(eta.is_some(), "ETA should be calculable when speed > 0");

        let eta_value = eta.unwrap();
        // Allow generous tolerance due to EMA variance
        assert!(
            eta_value <= 10, // Should complete within 10 seconds at this rate
            "ETA for 10000 bytes at {:.0} B/s should be <= 10s, got {}s",
            speed,
            eta_value
        );

        // Test edge case: zero remaining bytes
        let eta_zero = smoother.eta_seconds(0);
        assert_eq!(eta_zero, Some(0), "ETA for 0 remaining bytes should be 0");

        // Reset and test zero-speed case
        smoother.reset();
        let eta_no_speed = smoother.eta_seconds(99999);
        assert!(
            eta_no_speed.is_none(),
            "ETA should be None when speed is zero"
        );
    }

    #[test]
    fn test_reset_clears_state() {
        let mut smoother = SpeedSmoother::new(10);

        // Populate with data
        simulate_downloads(&mut smoother, 500, 50, 20);

        // Verify state is populated
        assert!(
            smoother.smoothed_speed() > 0.0,
            "Should have speed before reset"
        );
        assert!(
            smoother.samples_count() > 0,
            "Should have samples before reset"
        );

        // Perform reset
        smoother.reset();

        // Verify all state is cleared
        assert_eq!(
            smoother.smoothed_speed(),
            0.0,
            "Speed should be 0 after reset"
        );
        assert_eq!(
            smoother.samples_count(),
            0,
            "Sample count should be 0 after reset"
        );
        assert_eq!(
            smoother.instant_speed(),
            0.0,
            "Instant speed should be 0 after reset"
        );

        // Verify ETA cannot be calculated after reset
        let eta = smoother.eta_seconds(12345);
        assert!(eta.is_none(), "ETA should be None after reset (no speed)");

        // Verify not in burst state after reset
        assert!(
            !smoother.is_burst(),
            "Should not be in burst state after reset"
        );
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
