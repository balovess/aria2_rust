use super::{
    DEFAULT_IDLE_TIMEOUT, DEFAULT_INITIAL_RTO, DEFAULT_KEEPALIVE_INTERVAL, MAX_RETRANSMIT_ATTEMPTS,
    MAX_RTO, MIN_RTO,
};
use std::time::{Duration, Instant};

/// Retransmission scheduler for uTP
///
/// Manages packet retransmission with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetransmitScheduler {
    /// Base RTO (retransmission timeout)
    base_rto: Duration,
    /// Current RTO (may be backed off)
    current_rto: Duration,
    /// Number of consecutive timeouts
    timeout_count: u32,
    /// Maximum number of retransmit attempts
    max_attempts: u32,
    /// Backoff multiplier
    backoff_multiplier: f64,
    /// Maximum backoff
    max_backoff: f64,
}

impl Default for RetransmitScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl RetransmitScheduler {
    /// Create a new retransmit scheduler
    pub fn new() -> Self {
        Self {
            base_rto: DEFAULT_INITIAL_RTO,
            current_rto: DEFAULT_INITIAL_RTO,
            timeout_count: 0,
            max_attempts: MAX_RETRANSMIT_ATTEMPTS,
            backoff_multiplier: 2.0,
            max_backoff: 64.0,
        }
    }

    /// Create a scheduler with custom RTO
    pub fn with_rto(rto: Duration) -> Self {
        Self {
            base_rto: rto,
            current_rto: rto,
            ..Self::new()
        }
    }

    /// Get current RTO
    pub fn rto(&self) -> Duration {
        self.current_rto
    }

    /// Get base RTO
    pub fn base_rto(&self) -> Duration {
        self.base_rto
    }

    /// Get timeout count
    pub fn timeout_count(&self) -> u32 {
        self.timeout_count
    }

    /// Check if max attempts reached
    pub fn is_max_attempts_reached(&self) -> bool {
        self.timeout_count >= self.max_attempts
    }

    /// Record a timeout (triggers backoff)
    pub fn on_timeout(&mut self) {
        self.timeout_count += 1;

        // Calculate exponential backoff
        let backoff = self.backoff_multiplier.powi(self.timeout_count as i32);
        let clamped_backoff = backoff.min(self.max_backoff);

        let new_rto_us = (self.base_rto.as_micros() as f64 * clamped_backoff) as u64;
        self.current_rto = Duration::from_micros(new_rto_us).clamp(MIN_RTO, MAX_RTO);
    }

    /// Reset after successful ACK
    pub fn on_ack_received(&mut self) {
        self.timeout_count = 0;
        self.current_rto = self.base_rto;
    }

    /// Update base RTO based on RTT estimate
    pub fn update_rto(&mut self, srtt: Duration, rttvar: Duration) {
        // RTO = SRTT + 4 * RTTVAR
        let new_rto = srtt + 4 * rttvar;
        self.base_rto = new_rto.clamp(MIN_RTO, MAX_RTO);

        // Reset current RTO if no timeouts pending
        if self.timeout_count == 0 {
            self.current_rto = self.base_rto;
        }
    }

    /// Set maximum attempts
    pub fn set_max_attempts(&mut self, max: u32) {
        self.max_attempts = max;
    }

    /// Reset the scheduler
    pub fn reset(&mut self) {
        self.current_rto = self.base_rto;
        self.timeout_count = 0;
    }
}

/// Keepalive manager for uTP connections
///
/// Manages keepalive packet scheduling to maintain idle connections.
#[derive(Debug, Clone)]
pub struct KeepaliveManager {
    /// Keepalive interval
    interval: Duration,
    /// Time of last keepalive sent
    last_keepalive: Option<Instant>,
    /// Time of last activity (data sent/received)
    last_activity: Option<Instant>,
    /// Whether keepalive is enabled
    enabled: bool,
}

impl Default for KeepaliveManager {
    fn default() -> Self {
        Self::new()
    }
}

impl KeepaliveManager {
    /// Create a new keepalive manager
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_KEEPALIVE_INTERVAL,
            last_keepalive: None,
            last_activity: None,
            enabled: true,
        }
    }

    /// Create with custom interval
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            ..Self::new()
        }
    }

    /// Get keepalive interval
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Set keepalive interval
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Check if keepalive is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable/disable keepalive
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Record activity (data sent or received)
    pub fn record_activity(&mut self) {
        self.last_activity = Some(Instant::now());
    }

    /// Record keepalive sent
    pub fn record_keepalive_sent(&mut self) {
        self.last_keepalive = Some(Instant::now());
    }

    /// Check if keepalive should be sent
    pub fn should_send_keepalive(&self) -> bool {
        if !self.enabled {
            return false;
        }

        let now = Instant::now();

        // Check if we've been idle for the keepalive interval
        if let Some(last_activity) = self.last_activity
            && now.duration_since(last_activity) >= self.interval
        {
            return true;
        }

        // Check if we haven't sent a keepalive recently
        if let Some(last_keepalive) = self.last_keepalive {
            if now.duration_since(last_keepalive) >= self.interval {
                return true;
            }
        } else {
            // No keepalive sent yet, check if we've been idle
            if self.last_activity.is_none() {
                return true;
            }
        }

        false
    }

    /// Get time until next keepalive
    pub fn next_keepalive(&self) -> Option<Duration> {
        if !self.enabled {
            return None;
        }

        let now = Instant::now();

        // Calculate time since last activity
        let idle_time = self
            .last_activity
            .map_or(Duration::ZERO, |t| now.duration_since(t));

        // Calculate remaining time until keepalive
        if idle_time >= self.interval {
            Some(Duration::ZERO)
        } else {
            Some(self.interval - idle_time)
        }
    }

    /// Reset the manager
    pub fn reset(&mut self) {
        self.last_keepalive = None;
        self.last_activity = None;
    }
}

/// Idle timeout detector for uTP connections
///
/// Detects when connections have been idle too long and should be closed.
#[derive(Debug, Clone)]
pub struct IdleTimeoutDetector {
    /// Idle timeout duration
    timeout: Duration,
    /// Time of last activity
    last_activity: Option<Instant>,
    /// Whether timeout detection is enabled
    enabled: bool,
}

impl Default for IdleTimeoutDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleTimeoutDetector {
    /// Create a new idle timeout detector
    pub fn new() -> Self {
        Self {
            timeout: DEFAULT_IDLE_TIMEOUT,
            last_activity: None,
            enabled: true,
        }
    }

    /// Create with custom timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::new()
        }
    }

    /// Get idle timeout
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Set idle timeout
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Check if detection is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable/disable detection
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Record activity
    pub fn record_activity(&mut self) {
        self.last_activity = Some(Instant::now());
    }

    /// Check if connection has timed out
    pub fn is_timeout(&self) -> bool {
        if !self.enabled {
            return false;
        }

        if let Some(last_activity) = self.last_activity {
            last_activity.elapsed() >= self.timeout
        } else {
            // No activity recorded, consider as timed out
            true
        }
    }

    /// Get idle time
    pub fn idle_time(&self) -> Duration {
        self.last_activity.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Get remaining time until timeout
    pub fn remaining_time(&self) -> Option<Duration> {
        if !self.enabled {
            return None;
        }

        let idle = self.idle_time();
        if idle >= self.timeout {
            Some(Duration::ZERO)
        } else {
            Some(self.timeout - idle)
        }
    }

    /// Reset the detector
    pub fn reset(&mut self) {
        self.last_activity = Some(Instant::now());
    }
}
