//! LEDBAT (Low Extra Delay Background Transport) congestion control
//!
//! Implements LEDBAT as specified in RFC 6817 for uTP.
//! LEDBAT is a delay-based congestion control algorithm that aims to:
//! - Keep network queues small (target delay)
//! - Yield to standard TCP traffic
//! - Provide efficient background data transfer

use std::collections::VecDeque;
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

    /// Maximum segment size in bytes
    mss: u32,

    /// History of delay samples for base delay calculation
    delay_history: Vec<u64>,

    /// Maximum number of delay samples to keep
    max_history: usize,

    /// One minimum delay for each of the ten most recent one-minute windows.
    base_delay_history: VecDeque<Option<u64>>,

    /// Start time used to advance base-delay minute windows.
    base_delay_started: Instant,

    /// Current minute index in the base-delay history.
    base_delay_minute: u64,
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
            mss: DEFAULT_MSS,
            delay_history: Vec::with_capacity(60),
            max_history: 60, // ~1 minute of samples at 1 sample/second
            base_delay_history: VecDeque::from([None; 10]),
            base_delay_started: Instant::now(),
            base_delay_minute: 0,
        }
    }

    /// Create a new LEDBAT controller with custom MSS
    pub fn with_mss(mss: u32) -> Self {
        // Keep window arithmetic representable and packet division defined
        // when an embedding caller supplies an invalid MSS.
        let mss = mss.clamp(1, u32::MAX / LEDBAT_MAX_CWND);
        Self {
            cwnd: LEDBAT_MIN_CWND.saturating_mul(mss),
            mss,
            ..Self::new()
        }
    }

    /// Create a new LEDBAT controller with custom target delay
    pub fn with_target_delay(target_delay: Duration) -> Self {
        Self {
            target_delay_us: u64::try_from(target_delay.as_micros())
                .unwrap_or(u64::MAX)
                .max(1),
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
        let queuing_us = self
            .current_delay
            .saturating_sub(self.base_delay.unwrap_or(0));
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

    /// Process an ACK and update the LEDBAT congestion window.
    ///
    /// # Arguments
    /// * `timestamp_diff` - The timestamp difference from the ACK (in microseconds)
    /// * `bytes_acked` - Number of bytes acknowledged
    pub fn on_ack_received(&mut self, timestamp_diff: u64, bytes_acked: u32) {
        let flight_size_before_ack = self.bytes_in_flight;
        self.update_delay(timestamp_diff);
        let queuing_delay_us = self
            .current_delay
            .saturating_sub(self.base_delay.unwrap_or(self.current_delay));
        let off_target =
            (self.target_delay_us as f64 - queuing_delay_us as f64) / self.target_delay_us as f64;
        let delta = GAIN * off_target * f64::from(bytes_acked) * f64::from(self.mss)
            / f64::from(self.cwnd.max(1));
        let updated_cwnd = (f64::from(self.cwnd) + delta).max(0.0) as u32;
        let max_allowed_cwnd = flight_size_before_ack.saturating_add(self.mss);
        let min_cwnd = LEDBAT_MIN_CWND.saturating_mul(self.mss);
        let max_cwnd = LEDBAT_MAX_CWND.saturating_mul(self.mss);
        self.cwnd = updated_cwnd
            .min(max_allowed_cwnd)
            .max(min_cwnd)
            .min(max_cwnd);
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes_acked);
    }

    /// Update delay measurements
    ///
    /// This method can be called directly to feed delay measurements
    /// to the LEDBAT controller, which is useful for integrating
    /// delay data from received packets before ACK processing.
    ///
    /// # Arguments
    /// * `delay_us` - One-way delay measurement in microseconds
    pub fn update_delay(&mut self, delay_us: u64) {
        self.current_delay = delay_us;

        self.update_base_delay_at(delay_us, self.base_delay_started.elapsed().as_secs() / 60);

        self.delay_history.push(delay_us);
        if self.delay_history.len() > self.max_history {
            self.delay_history.remove(0);
        }
    }

    fn update_base_delay_at(&mut self, delay_us: u64, minute: u64) {
        while self.base_delay_minute < minute {
            self.base_delay_history.pop_front();
            self.base_delay_history.push_back(None);
            self.base_delay_minute += 1;
        }
        if let Some(current_minute) = self.base_delay_history.back_mut() {
            *current_minute =
                Some(current_minute.map_or(delay_us, |minimum| minimum.min(delay_us)));
        }
        self.base_delay = self.base_delay_history.iter().flatten().copied().min();
    }

    /// Handle timeout event
    ///
    /// Reduces congestion window significantly on timeout
    pub fn on_timeout(&mut self) {
        self.cwnd = self.mss;
    }

    /// Handle packet loss
    ///
    /// Reduces congestion window moderately on loss
    pub fn on_loss(&mut self) {
        // Reduce cwnd by half
        self.cwnd = (self.cwnd / 2).max(LEDBAT_MIN_CWND * self.mss);
    }

    /// Reset controller to initial state
    pub fn reset(&mut self) {
        self.cwnd = LEDBAT_MIN_CWND * self.mss;
        self.bytes_in_flight = 0;
        self.base_delay = None;
        self.current_delay = 0;
        self.last_send_time = None;
        self.delay_history.clear();
        self.base_delay_history = VecDeque::from([None; 10]);
        self.base_delay_started = Instant::now();
        self.base_delay_minute = 0;
    }

    /// Check if we're in slow start mode
    pub fn is_slow_start(&self) -> bool {
        false
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
#[path = "congestion/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "congestion/simulation.rs"]
mod simulation;
