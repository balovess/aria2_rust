//! uTP metrics estimation
//!
//! Implements RTT, delay, and bandwidth estimation for uTP congestion control.

use std::time::{Duration, Instant};

/// Default initial RTT estimate in milliseconds
const DEFAULT_INITIAL_RTT_MS: u64 = 100;

/// Alpha for RTT smoothing (EWMA weight)
const RTT_ALPHA: f64 = 0.125;

/// Beta for RTT variance smoothing
const RTT_BETA: f64 = 0.25;

/// Minimum congestion window size (in packets)
const MIN_CWND: u32 = 2;

/// RTT (Round-Trip Time) estimator using EWMA
///
/// Implements the standard TCP RTT estimation algorithm as described in RFC 6298,
/// adapted for uTP's microsecond-resolution timestamps.
#[derive(Debug, Clone)]
pub struct RttEstimator {
    /// Smoothed RTT in microseconds
    srtt: Option<u64>,
    /// RTT variance in microseconds
    rttvar: Option<u64>,
    /// Minimum RTT observed in microseconds
    min_rtt: Option<u64>,
    /// Last measurement time
    last_update: Option<Instant>,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    /// Create a new RTT estimator
    pub fn new() -> Self {
        Self {
            srtt: None,
            rttvar: None,
            min_rtt: None,
            last_update: None,
        }
    }

    /// Add a new RTT sample
    ///
    /// Uses EWMA to smooth RTT estimates:
    /// - SRTT = (1 - α) * SRTT + α * RTT_sample
    /// - RTTVAR = (1 - β) * RTTVAR + β * |SRTT - RTT_sample|
    pub fn add_sample(&mut self, rtt_us: u64) {
        // Update minimum RTT
        self.min_rtt = Some(self.min_rtt.map_or(rtt_us, |m| m.min(rtt_us)));

        // First sample: initialize SRTT and RTTVAR
        if self.srtt.is_none() {
            self.srtt = Some(rtt_us);
            self.rttvar = Some(rtt_us / 2);
        } else {
            // EWMA update
            let srtt = self.srtt.unwrap() as f64;
            let rttvar = self.rttvar.unwrap() as f64;
            let rtt = rtt_us as f64;

            let new_srtt = (1.0 - RTT_ALPHA) * srtt + RTT_ALPHA * rtt;
            let new_rttvar = (1.0 - RTT_BETA) * rttvar + RTT_BETA * (srtt - rtt).abs();

            self.srtt = Some(new_srtt as u64);
            self.rttvar = Some(new_rttvar as u64);
        }

        self.last_update = Some(Instant::now());
    }

    /// Get the smoothed RTT in microseconds
    pub fn srtt_us(&self) -> u64 {
        self.srtt.unwrap_or(DEFAULT_INITIAL_RTT_MS * 1000)
    }

    /// Get the smoothed RTT as Duration
    pub fn srtt(&self) -> Duration {
        Duration::from_micros(self.srtt_us())
    }

    /// Get the RTT variance in microseconds
    pub fn rttvar_us(&self) -> u64 {
        self.rttvar.unwrap_or(DEFAULT_INITIAL_RTT_MS * 500)
    }

    /// Get the minimum RTT observed in microseconds
    pub fn min_rtt_us(&self) -> Option<u64> {
        self.min_rtt
    }

    /// Get the minimum RTT as Duration
    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt.map(Duration::from_micros)
    }

    /// Calculate the retransmission timeout (RTO)
    ///
    /// RTO = SRTT + 4 * RTTVAR (with bounds)
    pub fn rto(&self) -> Duration {
        let srtt = self.srtt_us();
        let rttvar = self.rttvar_us();
        let rto_us = srtt.saturating_add(4 * rttvar);

        // Clamp RTO between 200ms and 2 seconds (per RFC 6298)
        let rto_clamped = rto_us.clamp(200_000, 2_000_000);
        Duration::from_micros(rto_clamped)
    }

    /// Check if we have enough samples for reliable estimates
    pub fn has_samples(&self) -> bool {
        self.srtt.is_some()
    }

    /// Reset the estimator
    pub fn reset(&mut self) {
        self.srtt = None;
        self.rttvar = None;
        self.min_rtt = None;
        self.last_update = None;
    }
}

/// One-way delay estimator for uTP
///
/// Estimates the one-way delay from timestamp differences in uTP packets.
/// This is used for LEDBAT-style congestion control.
#[derive(Debug, Clone)]
pub struct DelayEstimator {
    /// Base delay (minimum observed delay) in microseconds
    base_delay: Option<u64>,
    /// Current delay in microseconds
    current_delay: u64,
    /// Delay history for calculating base delay
    delay_history: Vec<u64>,
    /// Maximum history size
    max_history: usize,
    /// Target delay in microseconds (default 100ms for LEDBAT)
    target_delay_us: u64,
}

impl Default for DelayEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayEstimator {
    /// Create a new delay estimator
    pub fn new() -> Self {
        Self {
            base_delay: None,
            current_delay: 0,
            delay_history: Vec::with_capacity(60),
            max_history: 60,          // ~1 minute of samples at 1 sample/second
            target_delay_us: 100_000, // 100ms target delay
        }
    }

    /// Create a delay estimator with custom target delay
    pub fn with_target_delay(target_delay: Duration) -> Self {
        Self {
            target_delay_us: target_delay.as_micros() as u64,
            ..Self::new()
        }
    }

    /// Add a delay sample from timestamp difference
    ///
    /// The delay is calculated from the difference between the current time
    /// and the timestamp in the received packet.
    pub fn add_sample(&mut self, delay_us: u64) {
        self.current_delay = delay_us;

        // Add to history
        self.delay_history.push(delay_us);
        if self.delay_history.len() > self.max_history {
            self.delay_history.remove(0);
        }

        // Update base delay (minimum in history)
        self.base_delay = self.delay_history.iter().min().copied();
    }

    /// Get the current delay in microseconds
    pub fn current_delay_us(&self) -> u64 {
        self.current_delay
    }

    /// Get the current delay as Duration
    pub fn current_delay(&self) -> Duration {
        Duration::from_micros(self.current_delay)
    }

    /// Get the base delay (minimum observed) in microseconds
    pub fn base_delay_us(&self) -> Option<u64> {
        self.base_delay
    }

    /// Get the base delay as Duration
    pub fn base_delay(&self) -> Option<Duration> {
        self.base_delay.map(Duration::from_micros)
    }

    /// Get the queuing delay (current - base)
    ///
    /// This represents the time packets spend waiting in queues
    pub fn queuing_delay_us(&self) -> u64 {
        self.current_delay
            .saturating_sub(self.base_delay.unwrap_or(0))
    }

    /// Get the queuing delay as Duration
    pub fn queuing_delay(&self) -> Duration {
        Duration::from_micros(self.queuing_delay_us())
    }

    /// Get the target delay
    pub fn target_delay(&self) -> Duration {
        Duration::from_micros(self.target_delay_us)
    }

    /// Calculate how much we're above or below the target delay
    ///
    /// Returns positive if above target, negative if below
    pub fn delay_offset_us(&self) -> i64 {
        let queuing = self.queuing_delay_us() as i64;
        let target = self.target_delay_us as i64;
        queuing - target
    }

    /// Check if we're experiencing congestion (queuing delay > target)
    pub fn is_congested(&self) -> bool {
        self.queuing_delay_us() > self.target_delay_us
    }

    /// Reset the estimator
    pub fn reset(&mut self) {
        self.base_delay = None;
        self.current_delay = 0;
        self.delay_history.clear();
    }
}

/// Bandwidth estimator using moving average
///
/// Estimates available bandwidth based on data transfer rates.
#[derive(Debug, Clone)]
pub struct BandwidthEstimator {
    /// Bytes transferred in the current window
    bytes_in_window: u64,
    /// Time when current window started
    window_start: Option<Instant>,
    /// Window duration for averaging
    window_duration: Duration,
    /// Bandwidth samples (bytes per second)
    samples: Vec<f64>,
    /// Maximum number of samples to keep
    max_samples: usize,
    /// Smoothed bandwidth estimate (bytes per second)
    smoothed_bps: Option<f64>,
}

impl Default for BandwidthEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BandwidthEstimator {
    /// Create a new bandwidth estimator
    pub fn new() -> Self {
        Self {
            bytes_in_window: 0,
            window_start: None,
            window_duration: Duration::from_millis(100),
            samples: Vec::with_capacity(10),
            max_samples: 10,
            smoothed_bps: None,
        }
    }

    /// Create a bandwidth estimator with custom window duration
    pub fn with_window_duration(window: Duration) -> Self {
        Self {
            window_duration: window,
            ..Self::new()
        }
    }

    /// Record bytes transferred
    pub fn record_bytes(&mut self, bytes: u64) {
        let now = Instant::now();

        if self.window_start.is_none() {
            self.window_start = Some(now);
        }

        self.bytes_in_window += bytes;

        // Check if window has elapsed
        if let Some(start) = self.window_start
            && now.duration_since(start) >= self.window_duration
        {
            self.finalize_window();
        }
    }

    /// Finalize the current window and calculate bandwidth
    fn finalize_window(&mut self) {
        if let Some(start) = self.window_start {
            let elapsed = start.elapsed();
            if elapsed.as_secs_f64() > 0.0 {
                let bps = (self.bytes_in_window as f64) / elapsed.as_secs_f64();
                self.add_sample(bps);
            }
        }

        // Reset for next window
        self.bytes_in_window = 0;
        self.window_start = Some(Instant::now());
    }

    /// Add a bandwidth sample
    fn add_sample(&mut self, bps: f64) {
        self.samples.push(bps);
        if self.samples.len() > self.max_samples {
            self.samples.remove(0);
        }

        // Calculate smoothed estimate
        if !self.samples.is_empty() {
            let sum: f64 = self.samples.iter().sum();
            self.smoothed_bps = Some(sum / self.samples.len() as f64);
        }
    }

    /// Get the estimated bandwidth in bytes per second
    pub fn bytes_per_second(&self) -> Option<f64> {
        self.smoothed_bps
    }

    /// Get the estimated bandwidth in bits per second
    pub fn bits_per_second(&self) -> Option<f64> {
        self.smoothed_bps.map(|bps| bps * 8.0)
    }

    /// Get the estimated bandwidth as a human-readable string
    pub fn bandwidth_string(&self) -> String {
        if let Some(bps) = self.bits_per_second() {
            if bps >= 1_000_000_000.0 {
                format!("{:.2} Gbps", bps / 1_000_000_000.0)
            } else if bps >= 1_000_000.0 {
                format!("{:.2} Mbps", bps / 1_000_000.0)
            } else if bps >= 1_000.0 {
                format!("{:.2} Kbps", bps / 1_000.0)
            } else {
                format!("{:.0} bps", bps)
            }
        } else {
            "Unknown".to_string()
        }
    }

    /// Check if we have enough samples for a reliable estimate
    pub fn has_estimate(&self) -> bool {
        self.smoothed_bps.is_some()
    }

    /// Get the number of samples collected
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Reset the estimator
    pub fn reset(&mut self) {
        self.bytes_in_window = 0;
        self.window_start = None;
        self.samples.clear();
        self.smoothed_bps = None;
    }

    /// Force update (call periodically to ensure current window is finalized)
    pub fn update(&mut self) {
        if self.bytes_in_window > 0 {
            self.finalize_window();
        }
    }
}

/// Congestion window manager for uTP
///
/// Implements LEDBAT-style congestion control based on delay.
#[derive(Debug, Clone)]
pub struct CongestionController {
    /// Congestion window (in bytes)
    cwnd: u32,
    /// Slow start threshold
    ssthresh: u32,
    /// Bytes in flight
    bytes_in_flight: u32,
    /// Maximum window size
    max_wnd: u32,
    /// Whether we're in slow start
    in_slow_start: bool,
}

impl Default for CongestionController {
    fn default() -> Self {
        Self::new()
    }
}

impl CongestionController {
    /// Create a new congestion controller
    pub fn new() -> Self {
        Self {
            cwnd: 2 * 1500, // Start with 2 MSS
            ssthresh: u32::MAX,
            bytes_in_flight: 0,
            max_wnd: 1024 * 1024, // 1 MB max window
            in_slow_start: true,
        }
    }

    /// Get the current congestion window
    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Get the slow start threshold
    pub fn ssthresh(&self) -> u32 {
        self.ssthresh
    }

    /// Get bytes currently in flight
    pub fn bytes_in_flight(&self) -> u32 {
        self.bytes_in_flight
    }

    /// Check if we can send more data
    pub fn can_send(&self) -> bool {
        self.bytes_in_flight < self.cwnd
    }

    /// Get available window (in bytes)
    pub fn available_window(&self) -> u32 {
        self.cwnd.saturating_sub(self.bytes_in_flight)
    }

    /// Record data being sent
    pub fn on_send(&mut self, bytes: u32) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(bytes);
    }

    /// Record data being acknowledged
    pub fn on_ack(&mut self, bytes: u32, delay_offset_us: i64) {
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);

        // LEDBAT-style congestion control
        if self.in_slow_start {
            // Slow start: exponential increase
            self.cwnd = self.cwnd.saturating_add(bytes);
            if self.cwnd >= self.ssthresh {
                self.in_slow_start = false;
            }
        } else {
            // Congestion avoidance with delay-based control
            let gain = self.calculate_gain(delay_offset_us);
            self.cwnd = (self.cwnd as f64 * gain) as u32;
        }

        // Clamp to bounds
        self.cwnd = self.cwnd.clamp(MIN_CWND * 1500, self.max_wnd);
    }

    /// Calculate the gain factor based on delay offset
    fn calculate_gain(&self, delay_offset_us: i64) -> f64 {
        const TARGET_DELAY_US: i64 = 100_000; // 100ms
        const GAIN: f64 = 1.0 / TARGET_DELAY_US as f64;

        if delay_offset_us > 0 {
            // Above target: reduce window
            1.0 - GAIN * delay_offset_us as f64 / 1000.0
        } else {
            // Below target: increase window
            1.0 + GAIN * (-delay_offset_us) as f64 / 1000.0
        }
    }

    /// Handle packet loss
    pub fn on_loss(&mut self) {
        // Reduce ssthresh and enter congestion avoidance
        self.ssthresh = self.cwnd / 2;
        self.cwnd = self.ssthresh.max(MIN_CWND * 1500);
        self.in_slow_start = false;
    }

    /// Handle timeout
    pub fn on_timeout(&mut self) {
        // Reset to minimum window
        self.ssthresh = self.cwnd / 2;
        self.cwnd = MIN_CWND * 1500;
        self.in_slow_start = true;
        self.bytes_in_flight = 0;
    }

    /// Reset the controller
    pub fn reset(&mut self) {
        self.cwnd = 2 * 1500;
        self.ssthresh = u32::MAX;
        self.bytes_in_flight = 0;
        self.in_slow_start = true;
    }
}

#[cfg(test)]
#[path = "metrics/tests.rs"]
mod tests;
