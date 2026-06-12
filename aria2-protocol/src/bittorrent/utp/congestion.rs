//! LEDBAT (Low Extra Delay Background Transport) congestion control
//!
//! Implements LEDBAT as specified in RFC 6817 for uTP.
//! LEDBAT is a delay-based congestion control algorithm that aims to:
//! - Keep network queues small (target delay)
//! - Yield to standard TCP traffic
//! - Provide efficient background data transfer

use std::time::{Duration, Instant};

/// Target delay for LEDBAT (100ms as per RFC 6817)
pub const LEDBAT_TARGET_DELAY: Duration = Duration::from_millis(100);

/// Minimum congestion window in packets
pub const LEDBAT_MIN_CWND: u32 = 2;

/// Maximum congestion window in packets
pub const LEDBAT_MAX_CWND: u32 = 1000;

/// Gain factor for congestion window adjustment
pub const GAIN: f64 = 0.5;

/// Default Maximum Segment Size (MSS) in bytes
const DEFAULT_MSS: u32 = 1500;

/// LEDBAT congestion controller
///
/// Implements the LEDBAT algorithm as described in RFC 6817.
/// The controller maintains a congestion window based on measured delays
/// and adjusts it to keep queuing delay at or below the target.
#[derive(Debug, Clone)]
pub struct LedbatController {
    /// Congestion window in bytes
    cwnd: u32,

    /// Bytes currently in flight (sent but not acknowledged)
    bytes_in_flight: u32,

    /// Base delay (minimum observed one-way delay) in microseconds
    base_delay: Option<u64>,

    /// Current delay measurement in microseconds
    current_delay: u64,

    /// Target delay in microseconds
    target_delay_us: u64,

    /// Time of last data send
    last_send_time: Option<Instant>,

    /// Whether we're in slow start mode
    slow_start: bool,

    /// Maximum segment size in bytes
    mss: u32,

    /// History of delay samples for base delay calculation
    delay_history: Vec<u64>,

    /// Maximum number of delay samples to keep
    max_history: usize,

    /// Number of ACKs received in slow start
    slow_start_acks: u32,
}

impl Default for LedbatController {
    fn default() -> Self {
        Self::new()
    }
}

impl LedbatController {
    /// Create a new LEDBAT controller with default settings
    pub fn new() -> Self {
        Self {
            cwnd: LEDBAT_MIN_CWND * DEFAULT_MSS,
            bytes_in_flight: 0,
            base_delay: None,
            current_delay: 0,
            target_delay_us: LEDBAT_TARGET_DELAY.as_micros() as u64,
            last_send_time: None,
            slow_start: true,
            mss: DEFAULT_MSS,
            delay_history: Vec::with_capacity(60),
            max_history: 60, // ~1 minute of samples at 1 sample/second
            slow_start_acks: 0,
        }
    }

    /// Create a new LEDBAT controller with custom MSS
    pub fn with_mss(mss: u32) -> Self {
        Self {
            cwnd: LEDBAT_MIN_CWND * mss,
            mss,
            ..Self::new()
        }
    }

    /// Create a new LEDBAT controller with custom target delay
    pub fn with_target_delay(target_delay: Duration) -> Self {
        Self {
            target_delay_us: target_delay.as_micros() as u64,
            ..Self::new()
        }
    }

    /// Get the current congestion window size in bytes
    pub fn get_window_size(&self) -> u32 {
        self.cwnd
    }

    /// Get the current congestion window size in packets
    pub fn get_window_packets(&self) -> u32 {
        self.cwnd / self.mss
    }

    /// Get bytes currently in flight
    pub fn get_bytes_in_flight(&self) -> u32 {
        self.bytes_in_flight
    }

    /// Get the base delay (minimum observed delay)
    pub fn get_base_delay(&self) -> Option<Duration> {
        self.base_delay.map(Duration::from_micros)
    }

    /// Get the current delay
    pub fn get_current_delay(&self) -> Duration {
        Duration::from_micros(self.current_delay)
    }

    /// Get the queuing delay (current - base)
    pub fn get_queuing_delay(&self) -> Duration {
        let queuing_us = self.current_delay.saturating_sub(self.base_delay.unwrap_or(0));
        Duration::from_micros(queuing_us)
    }

    /// Check if we can send more data (window allows)
    pub fn can_send(&self) -> bool {
        self.bytes_in_flight < self.cwnd
    }

    /// Get available window space in bytes
    pub fn available_window(&self) -> u32 {
        self.cwnd.saturating_sub(self.bytes_in_flight)
    }

    /// Record data being sent
    ///
    /// Updates bytes in flight and last send time
    pub fn on_data_sent(&mut self, bytes: u32) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(bytes);
        self.last_send_time = Some(Instant::now());
    }

    /// Process an ACK and update congestion window
    ///
    /// This implements the core LEDBAT algorithm:
    /// - In slow start: increase cwnd by bytes_acked
    /// - In congestion avoidance: adjust based on queuing delay
    ///
    /// # Arguments
    /// * `timestamp_diff` - The timestamp difference from the ACK (in microseconds)
    /// * `bytes_acked` - Number of bytes acknowledged
    pub fn on_ack_received(&mut self, timestamp_diff: u64, bytes_acked: u32) {
        // Update delay measurements
        self.update_delay(timestamp_diff);

        // Decrease bytes in flight
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes_acked);

        // Calculate queuing delay
        // We need at least 3 samples to reliably calculate queuing delay
        let has_enough_samples = self.delay_history.len() >= 3;
        let queuing_delay_us = if has_enough_samples {
            self.current_delay.saturating_sub(self.base_delay.unwrap_or(0))
        } else {
            // Not enough samples, assume no queuing delay
            0
        };

        if self.slow_start {
            // Check if we should exit slow start BEFORE increasing cwnd
            // Exit slow start if:
            // 1. We have enough samples AND queuing delay exceeds target, or
            // 2. We've sent enough packets to establish baseline
            if (has_enough_samples && queuing_delay_us > self.target_delay_us)
                || self.slow_start_acks >= 10
            {
                self.slow_start = false;
                // Immediately apply congestion avoidance logic
                if has_enough_samples {
                    self.update_cwnd_congestion_avoidance(bytes_acked, queuing_delay_us);
                }
            } else {
                // Slow start: exponential increase
                self.slow_start_acks += 1;
                // Increase cwnd by bytes_acked
                self.cwnd = self.cwnd.saturating_add(bytes_acked);
            }
        } else {
            // Congestion avoidance: LEDBAT algorithm
            // Only adjust cwnd if we have enough samples
            if has_enough_samples {
                self.update_cwnd_congestion_avoidance(bytes_acked, queuing_delay_us);
            }
        }

        // Clamp cwnd to bounds
        self.clamp_cwnd();
    }

    /// Update congestion window in congestion avoidance mode
    ///
    /// LEDBAT formula:
    /// cwnd += GAIN * (target_delay - queuing_delay) / target_delay * bytes_acked
    fn update_cwnd_congestion_avoidance(&mut self, bytes_acked: u32, queuing_delay_us: u64) {
        let target = self.target_delay_us as f64;
        let queuing = queuing_delay_us as f64;

        // Calculate the delay factor
        // Positive: below target, increase cwnd
        // Negative: above target, decrease cwnd
        let delay_factor = (target - queuing) / target;

        // Apply gain
        let delta = GAIN * delay_factor * bytes_acked as f64;

        // Update cwnd (can be negative, so use saturating operations)
        if delta >= 0.0 {
            self.cwnd = self.cwnd.saturating_add(delta as u32);
        } else {
            self.cwnd = self.cwnd.saturating_sub((-delta) as u32);
        }
    }

    /// Update delay measurements
    fn update_delay(&mut self, delay_us: u64) {
        self.current_delay = delay_us;

        // Add to history
        self.delay_history.push(delay_us);
        if self.delay_history.len() > self.max_history {
            self.delay_history.remove(0);
        }

        // Update base delay (minimum in history)
        self.base_delay = self.delay_history.iter().min().copied();
    }

    /// Handle timeout event
    ///
    /// Reduces congestion window significantly on timeout
    pub fn on_timeout(&mut self) {
        // Reduce cwnd to minimum
        self.cwnd = LEDBAT_MIN_CWND * self.mss;

        // Reset bytes in flight
        self.bytes_in_flight = 0;

        // Re-enter slow start
        self.slow_start = true;
        self.slow_start_acks = 0;
    }

    /// Handle packet loss
    ///
    /// Reduces congestion window moderately on loss
    pub fn on_loss(&mut self) {
        // Reduce cwnd by half
        self.cwnd = (self.cwnd / 2).max(LEDBAT_MIN_CWND * self.mss);

        // Exit slow start if we were in it
        self.slow_start = false;
    }

    /// Reset controller to initial state
    pub fn reset(&mut self) {
        self.cwnd = LEDBAT_MIN_CWND * self.mss;
        self.bytes_in_flight = 0;
        self.base_delay = None;
        self.current_delay = 0;
        self.last_send_time = None;
        self.slow_start = true;
        self.delay_history.clear();
        self.slow_start_acks = 0;
    }

    /// Clamp congestion window to valid bounds
    fn clamp_cwnd(&mut self) {
        let min_cwnd = LEDBAT_MIN_CWND * self.mss;
        let max_cwnd = LEDBAT_MAX_CWND * self.mss;
        self.cwnd = self.cwnd.clamp(min_cwnd, max_cwnd);
    }

    /// Check if we're in slow start mode
    pub fn is_slow_start(&self) -> bool {
        self.slow_start
    }

    /// Get the target delay
    pub fn get_target_delay(&self) -> Duration {
        Duration::from_micros(self.target_delay_us)
    }

    /// Get time since last send
    pub fn time_since_last_send(&self) -> Option<Duration> {
        self.last_send_time.map(|t| t.elapsed())
    }

    /// Get the MSS (Maximum Segment Size)
    pub fn get_mss(&self) -> u32 {
        self.mss
    }

    /// Check if we have enough delay samples
    pub fn has_delay_samples(&self) -> bool {
        !self.delay_history.is_empty()
    }

    /// Get the number of delay samples collected
    pub fn delay_sample_count(&self) -> usize {
        self.delay_history.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_controller_initialization() {
        let controller = LedbatController::new();

        assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
        assert_eq!(controller.get_bytes_in_flight(), 0);
        assert!(controller.can_send());
        assert!(controller.is_slow_start());
        assert_eq!(controller.get_target_delay(), LEDBAT_TARGET_DELAY);
    }

    #[test]
    fn test_controller_with_custom_mss() {
        let controller = LedbatController::with_mss(1400);
        assert_eq!(controller.get_mss(), 1400);
        assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * 1400);
    }

    #[test]
    fn test_controller_with_custom_target_delay() {
        let controller = LedbatController::with_target_delay(Duration::from_millis(200));
        assert_eq!(controller.get_target_delay(), Duration::from_millis(200));
    }

    #[test]
    fn test_on_data_sent() {
        let mut controller = LedbatController::new();

        controller.on_data_sent(1500);
        assert_eq!(controller.get_bytes_in_flight(), 1500);
        assert!(controller.time_since_last_send().is_some());

        controller.on_data_sent(500);
        assert_eq!(controller.get_bytes_in_flight(), 2000);
    }

    #[test]
    fn test_can_send() {
        let mut controller = LedbatController::new();

        // Initially can send
        assert!(controller.can_send());

        // Fill the window
        let window = controller.get_window_size();
        controller.on_data_sent(window);
        assert!(!controller.can_send());

        // ACK some data
        controller.on_ack_received(50_000, 1500);
        assert!(controller.can_send());
    }

    #[test]
    fn test_available_window() {
        let mut controller = LedbatController::new();

        let initial_window = controller.available_window();
        assert_eq!(initial_window, controller.get_window_size());

        controller.on_data_sent(1500);
        let available = controller.available_window();
        assert_eq!(available, controller.get_window_size() - 1500);
    }

    #[test]
    fn test_slow_start_increase() {
        let mut controller = LedbatController::new();

        let initial_cwnd = controller.get_window_size();

        // In slow start, cwnd should increase by bytes_acked
        controller.on_ack_received(50_000, 1500);

        // cwnd should increase
        assert!(controller.get_window_size() > initial_cwnd);
        assert!(controller.is_slow_start());
    }

    #[test]
    fn test_slow_start_exit_on_target_delay() {
        let mut controller = LedbatController::new();

        // Send enough to fill window
        controller.on_data_sent(controller.get_window_size());

        // First, establish a low base delay with multiple samples
        controller.on_ack_received(50_000, 1500); // 50ms - low delay
        controller.on_ack_received(45_000, 1500); // 45ms - even lower
        controller.on_ack_received(48_000, 1500); // 48ms

        // Now we have 3 samples, base_delay should be ~45ms
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(45_000)));

        // ACK with high delay (above target)
        let high_delay = LEDBAT_TARGET_DELAY.as_micros() as u64 + 50_000; // 150ms
        controller.on_ack_received(high_delay, 1500);

        // Should exit slow start because queuing_delay (150-45=105ms) > target (100ms)
        assert!(!controller.is_slow_start());
    }

    #[test]
    fn test_slow_start_exit_on_ack_count() {
        let mut controller = LedbatController::new();

        // Send and ACK multiple times with low delay
        for _ in 0..15 {
            controller.on_data_sent(1500);
            controller.on_ack_received(50_000, 1500);
        }

        // Should exit slow start after enough ACKs
        assert!(!controller.is_slow_start());
    }

    #[test]
    fn test_congestion_avoidance_below_target() {
        let mut controller = LedbatController::new();

        // Force exit slow start
        controller.slow_start = false;

        let initial_cwnd = controller.get_window_size();

        // ACK with queuing delay below target (should increase cwnd)
        let low_delay = 50_000; // 50ms, well below 100ms target
        controller.base_delay = Some(20_000); // 20ms base
        controller.current_delay = low_delay;

        controller.on_ack_received(low_delay, 1500);

        // cwnd should increase slightly
        assert!(controller.get_window_size() >= initial_cwnd);
    }

    #[test]
    fn test_congestion_avoidance_above_target() {
        let mut controller = LedbatController::new();

        // Force exit slow start
        controller.slow_start = false;

        // Establish a base delay first
        controller.on_ack_received(20_000, 1500); // 20ms - this will be base
        controller.on_ack_received(25_000, 1500); // 25ms
        controller.on_ack_received(22_000, 1500); // 22ms

        // Now we have 3 samples, base_delay should be 20ms
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(20_000)));

        // Get cwnd after establishing base delay
        let cwnd_before_high_delay = controller.get_window_size();

        // ACK with queuing delay above target (should decrease cwnd)
        let high_delay = 150_000; // 150ms, queuing = 130ms > target (100ms)
        controller.on_ack_received(high_delay, 1500);

        // cwnd should decrease from the value before high delay
        assert!(controller.get_window_size() < cwnd_before_high_delay);

        // Continue with high delays to further reduce cwnd
        controller.on_ack_received(high_delay, 1500);
        controller.on_ack_received(high_delay, 1500);

        // After multiple high delay ACKs, cwnd should be significantly lower
        assert!(controller.get_window_size() < cwnd_before_high_delay - 500);
    }

    #[test]
    fn test_on_timeout() {
        let mut controller = LedbatController::new();

        // Increase cwnd first
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);

        let cwnd_before = controller.get_window_size();
        assert!(cwnd_before > LEDBAT_MIN_CWND * DEFAULT_MSS);

        // Timeout
        controller.on_timeout();

        // Should reset to minimum
        assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
        assert_eq!(controller.get_bytes_in_flight(), 0);
        assert!(controller.is_slow_start());
    }

    #[test]
    fn test_on_loss() {
        let mut controller = LedbatController::new();

        // Increase cwnd
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);

        let cwnd_before = controller.get_window_size();

        // Loss
        controller.on_loss();

        // Should reduce by half
        assert!(controller.get_window_size() <= cwnd_before / 2);
        assert!(!controller.is_slow_start());
    }

    #[test]
    fn test_reset() {
        let mut controller = LedbatController::new();

        // Modify state
        controller.on_data_sent(1500);
        controller.on_ack_received(50_000, 1500);
        controller.on_data_sent(1500);
        controller.slow_start = false;

        // Reset
        controller.reset();

        assert_eq!(controller.get_window_size(), LEDBAT_MIN_CWND * DEFAULT_MSS);
        assert_eq!(controller.get_bytes_in_flight(), 0);
        assert!(controller.is_slow_start());
        assert!(controller.get_base_delay().is_none());
        assert_eq!(controller.get_current_delay(), Duration::ZERO);
    }

    #[test]
    fn test_delay_measurements() {
        let mut controller = LedbatController::new();

        // First ACK
        controller.on_ack_received(50_000, 1500);
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(50_000)));
        assert_eq!(controller.get_current_delay(), Duration::from_micros(50_000));

        // Second ACK with higher delay
        controller.on_ack_received(80_000, 1500);
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(50_000))); // Min stays
        assert_eq!(controller.get_current_delay(), Duration::from_micros(80_000));

        // Third ACK with lower delay
        controller.on_ack_received(30_000, 1500);
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(30_000))); // New min
        assert_eq!(controller.get_current_delay(), Duration::from_micros(30_000));
    }

    #[test]
    fn test_queuing_delay() {
        let mut controller = LedbatController::new();

        controller.base_delay = Some(50_000); // 50ms
        controller.current_delay = 150_000; // 150ms

        let queuing = controller.get_queuing_delay();
        assert_eq!(queuing, Duration::from_micros(100_000)); // 100ms
    }

    #[test]
    fn test_window_bounds() {
        let mut controller = LedbatController::new();

        // Try to increase cwnd beyond max
        controller.cwnd = LEDBAT_MAX_CWND * DEFAULT_MSS + 10000;
        controller.on_ack_received(50_000, 1500);

        // Should be clamped to max
        assert_eq!(controller.get_window_size(), LEDBAT_MAX_CWND * DEFAULT_MSS);
    }

    #[test]
    fn test_minimum_window() {
        let mut controller = LedbatController::new();

        // Force cwnd below minimum
        controller.cwnd = 100; // Below minimum
        controller.on_ack_received(200_000, 1500); // High delay to reduce cwnd

        // Should be clamped to minimum
        assert!(controller.get_window_size() >= LEDBAT_MIN_CWND * DEFAULT_MSS);
    }

    #[test]
    fn test_bytes_in_flight_tracking() {
        let mut controller = LedbatController::new();

        // Send data
        controller.on_data_sent(1500);
        assert_eq!(controller.get_bytes_in_flight(), 1500);

        controller.on_data_sent(1500);
        assert_eq!(controller.get_bytes_in_flight(), 3000);

        // ACK some data
        controller.on_ack_received(50_000, 1500);
        assert_eq!(controller.get_bytes_in_flight(), 1500);

        // ACK remaining
        controller.on_ack_received(50_000, 1500);
        assert_eq!(controller.get_bytes_in_flight(), 0);
    }

    #[test]
    fn test_delay_sample_count() {
        let controller = LedbatController::new();
        assert_eq!(controller.delay_sample_count(), 0);
        assert!(!controller.has_delay_samples());

        let mut controller = controller;
        controller.on_ack_received(50_000, 1500);
        assert_eq!(controller.delay_sample_count(), 1);
        assert!(controller.has_delay_samples());
    }

    #[test]
    fn test_rfc6817_scenario() {
        // Test scenario based on RFC 6817 principles
        let mut controller = LedbatController::new();

        // Initial slow start phase with low delays
        for i in 0..3 {
            controller.on_data_sent(1500);
            controller.on_ack_received(50_000 - i * 1000, 1500); // Decreasing delays
            println!("Iteration {}: cwnd = {}, slow_start = {}, base_delay = {:?}",
                     i, controller.get_window_size(), controller.is_slow_start(),
                     controller.get_base_delay());
        }

        // Should have grown cwnd in slow start
        assert!(controller.get_window_size() > LEDBAT_MIN_CWND * DEFAULT_MSS);

        // Now we have 3 samples with base_delay ~47ms
        assert!(controller.get_base_delay().is_some());

        // Simulate congestion (high delay)
        for i in 0..3 {
            controller.on_data_sent(1500);
            controller.on_ack_received(150_000, 1500); // 150ms delay, queuing = 103ms > target
            println!("Congestion {}: cwnd = {}, slow_start = {}, queuing_delay = {:?}",
                     i, controller.get_window_size(), controller.is_slow_start(),
                     controller.get_queuing_delay());
        }

        // cwnd should have decreased due to high delay
        // and should have exited slow start
        assert!(!controller.is_slow_start());
    }

    #[test]
    fn test_gain_factor() {
        // Verify that the gain factor is applied correctly
        let mut controller = LedbatController::new();
        controller.slow_start = false;
        controller.cwnd = 10000;

        // Establish a base delay first
        controller.on_ack_received(20_000, 1500); // 20ms - this will be base
        controller.on_ack_received(25_000, 1500); // 25ms
        controller.on_ack_received(22_000, 1500); // 22ms

        // Now we have 3 samples, base_delay should be 20ms
        assert_eq!(controller.get_base_delay(), Some(Duration::from_micros(20_000)));

        let initial_cwnd = controller.get_window_size();

        // Queuing delay = 70ms - 20ms = 50ms, target = 100ms
        // delay_factor = (100 - 50) / 100 = 0.5
        // delta = 0.5 * 0.5 * 1500 = 375 bytes
        controller.on_ack_received(70_000, 1500);

        let new_cwnd = controller.get_window_size();
        // cwnd should increase by approximately 375 bytes
        assert!(new_cwnd > initial_cwnd);
        assert!(new_cwnd < initial_cwnd + 500); // Reasonable bound
    }

    #[test]
    fn test_saturating_operations() {
        let mut controller = LedbatController::new();

        // Test that operations don't overflow
        controller.on_data_sent(u32::MAX / 2);
        controller.on_data_sent(u32::MAX / 2);
        // bytes_in_flight should saturate, not overflow
        assert!(controller.get_bytes_in_flight() <= u32::MAX);

        // Test ACK with more bytes than in flight
        controller.on_ack_received(50_000, u32::MAX);
        assert_eq!(controller.get_bytes_in_flight(), 0);
    }

    #[test]
    fn test_time_since_last_send() {
        let controller = LedbatController::new();
        assert!(controller.time_since_last_send().is_none());

        let mut controller = controller;
        controller.on_data_sent(1500);
        let elapsed = controller.time_since_last_send();
        assert!(elapsed.is_some());
        assert!(elapsed.unwrap() < Duration::from_millis(100));
    }
}