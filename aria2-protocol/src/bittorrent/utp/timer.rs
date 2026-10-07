//! uTP timer management
//!
//! Implements timeout management, retransmission scheduling, and keep-alive timers
//! for uTP connections as specified in BEP 29.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default initial retransmission timeout
const DEFAULT_INITIAL_RTO: Duration = Duration::from_millis(1000);

/// Minimum retransmission timeout
const MIN_RTO: Duration = Duration::from_millis(200);

/// Maximum retransmission timeout
const MAX_RTO: Duration = Duration::from_secs(2);

/// Default keepalive interval
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Default connection timeout
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default idle timeout
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of retransmission attempts
const MAX_RETRANSMIT_ATTEMPTS: u32 = 5;

/// Timer types for uTP connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerType {
    /// Connection establishment timeout
    ConnectTimeout,
    /// Retransmission timeout for a specific sequence number
    Retransmit(u16),
    /// Keepalive timer
    Keepalive,
    /// Idle connection timeout
    IdleTimeout,
}

impl std::fmt::Display for TimerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerType::ConnectTimeout => write!(f, "ConnectTimeout"),
            TimerType::Retransmit(seq) => write!(f, "Retransmit(seq={})", seq),
            TimerType::Keepalive => write!(f, "Keepalive"),
            TimerType::IdleTimeout => write!(f, "IdleTimeout"),
        }
    }
}

/// Timer entry for tracking expiration
#[derive(Debug, Clone)]
struct TimerEntry {
    /// When the timer expires
    expires_at: Instant,
    /// Duration of the timer
    duration: Duration,
    /// Number of times this timer has fired (for retransmit)
    fire_count: u32,
}

impl TimerEntry {
    /// Create a new timer entry
    fn new(duration: Duration) -> Self {
        Self {
            expires_at: Instant::now() + duration,
            duration,
            fire_count: 0,
        }
    }

    /// Get remaining time until expiration
    fn remaining(&self) -> Duration {
        let now = Instant::now();
        if now >= self.expires_at {
            Duration::ZERO
        } else {
            self.expires_at - now
        }
    }

    /// Reset the timer (for retransmit timers)
    fn reset(&mut self, duration: Duration) {
        self.expires_at = Instant::now() + duration;
        self.duration = duration;
        self.fire_count += 1;
    }

    /// Check if this is a retransmit timer that has exceeded max attempts
    fn is_max_attempts_reached(&self, timer_type: TimerType) -> bool {
        matches!(timer_type, TimerType::Retransmit(_)) && self.fire_count >= MAX_RETRANSMIT_ATTEMPTS
    }
}

/// Timer manager for uTP connections
///
/// Manages all timers for multiple connections, providing:
/// - Connection timeout detection
/// - Retransmission scheduling with exponential backoff
/// - Keepalive scheduling
/// - Idle timeout detection
#[derive(Debug)]
pub struct TimerManager {
    /// Active timers indexed by (conn_id, timer_type)
    timers: HashMap<(u16, TimerType), TimerEntry>,
    /// Default timeouts
    default_connect_timeout: Duration,
    default_idle_timeout: Duration,
    default_keepalive_interval: Duration,
    /// Exponential backoff multiplier for retransmits
    backoff_multiplier: f64,
    /// Maximum backoff multiplier
    max_backoff: f64,
}

impl Default for TimerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerManager {
    /// Create a new timer manager
    pub fn new() -> Self {
        Self {
            timers: HashMap::new(),
            default_connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            default_idle_timeout: DEFAULT_IDLE_TIMEOUT,
            default_keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            backoff_multiplier: 2.0,
            max_backoff: 64.0,
        }
    }

    /// Create a timer manager with custom defaults
    pub fn with_defaults(
        connect_timeout: Duration,
        idle_timeout: Duration,
        keepalive_interval: Duration,
    ) -> Self {
        Self {
            timers: HashMap::new(),
            default_connect_timeout: connect_timeout,
            default_idle_timeout: idle_timeout,
            default_keepalive_interval: keepalive_interval,
            backoff_multiplier: 2.0,
            max_backoff: 64.0,
        }
    }

    /// Set a timer for a connection
    ///
    /// # Arguments
    /// * `conn_id` - Connection ID
    /// * `timer_type` - Type of timer
    /// * `duration` - Timer duration
    pub fn set_timer(&mut self, conn_id: u16, timer_type: TimerType, duration: Duration) {
        let key = (conn_id, timer_type);
        self.timers.insert(key, TimerEntry::new(duration));
    }

    /// Cancel a specific timer
    ///
    /// # Arguments
    /// * `conn_id` - Connection ID
    /// * `timer_type` - Type of timer to cancel
    pub fn cancel_timer(&mut self, conn_id: u16, timer_type: TimerType) {
        let key = (conn_id, timer_type);
        self.timers.remove(&key);
    }

    /// Cancel all timers for a connection
    ///
    /// # Arguments
    /// * `conn_id` - Connection ID
    pub fn cancel_all_timers(&mut self, conn_id: u16) {
        // Find all timer types for this connection
        let timer_types: Vec<TimerType> = self
            .timers
            .keys()
            .filter(|(id, _)| *id == conn_id)
            .map(|(_, t)| *t)
            .collect();

        // Remove all timers for this connection
        for timer_type in timer_types {
            self.cancel_timer(conn_id, timer_type);
        }
    }

    /// Get all expired timers
    ///
    /// Returns list of (conn_id, timer_type) for expired timers.
    /// Expired timers are removed from the manager.
    pub fn get_expired_timers(&mut self) -> Vec<(u16, TimerType)> {
        let mut expired = Vec::new();
        let now = Instant::now();

        // Collect expired keys first
        let expired_keys: Vec<(u16, TimerType)> = self
            .timers
            .iter()
            .filter(|(_, entry)| entry.expires_at <= now)
            .map(|(key, _)| *key)
            .collect();

        // Process expired timers
        for key in expired_keys {
            expired.push(key);

            if let Some(entry) = self.timers.get(&key) {
                // For retransmit timers, check if we should reset or remove
                if matches!(key.1, TimerType::Retransmit(_)) {
                    if entry.is_max_attempts_reached(key.1) {
                        // Max attempts reached, remove timer
                        self.timers.remove(&key);
                    } else {
                        // Reset with exponential backoff
                        let new_duration = self.calculate_backoff(entry.duration, entry.fire_count);
                        if let Some(new_entry) = self.timers.get_mut(&key) {
                            new_entry.reset(new_duration);
                        }
                    }
                } else {
                    // Remove one-shot timers
                    self.timers.remove(&key);
                }
            }
        }

        expired
    }

    /// Calculate exponential backoff for retransmit
    fn calculate_backoff(&self, base_duration: Duration, fire_count: u32) -> Duration {
        let backoff = self.backoff_multiplier.powi(fire_count as i32);
        let clamped_backoff = backoff.min(self.max_backoff);

        let new_duration_us = (base_duration.as_micros() as f64 * clamped_backoff) as u64;
        Duration::from_micros(new_duration_us).clamp(MIN_RTO, MAX_RTO)
    }

    /// Check if a specific timer exists
    pub fn has_timer(&self, conn_id: u16, timer_type: TimerType) -> bool {
        self.timers.contains_key(&(conn_id, timer_type))
    }

    /// Get remaining time for a timer
    pub fn remaining_time(&self, conn_id: u16, timer_type: TimerType) -> Option<Duration> {
        self.timers
            .get(&(conn_id, timer_type))
            .map(|e| e.remaining())
    }

    /// Get the next timer to expire
    pub fn next_timer(&self) -> Option<(u16, TimerType, Duration)> {
        self.timers
            .iter()
            .min_by_key(|(_, entry)| entry.expires_at)
            .map(|(key, entry)| (key.0, key.1, entry.remaining()))
    }

    /// Get number of active timers
    pub fn timer_count(&self) -> usize {
        self.timers.len()
    }

    /// Get number of timers for a specific connection
    pub fn connection_timer_count(&self, conn_id: u16) -> usize {
        self.timers.keys().filter(|(id, _)| *id == conn_id).count()
    }

    /// Clear all timers
    pub fn clear(&mut self) {
        self.timers.clear();
    }

    /// Get default connection timeout
    pub fn default_connect_timeout(&self) -> Duration {
        self.default_connect_timeout
    }

    /// Get default idle timeout
    pub fn default_idle_timeout(&self) -> Duration {
        self.default_idle_timeout
    }

    /// Get default keepalive interval
    pub fn default_keepalive_interval(&self) -> Duration {
        self.default_keepalive_interval
    }

    /// Set default connection timeout
    pub fn set_default_connect_timeout(&mut self, timeout: Duration) {
        self.default_connect_timeout = timeout;
    }

    /// Set default idle timeout
    pub fn set_default_idle_timeout(&mut self, timeout: Duration) {
        self.default_idle_timeout = timeout;
    }

    /// Set default keepalive interval
    pub fn set_default_keepalive_interval(&mut self, interval: Duration) {
        self.default_keepalive_interval = interval;
    }

    /// Get fire count for a retransmit timer
    pub fn retransmit_count(&self, conn_id: u16, seq_nr: u16) -> Option<u32> {
        self.timers
            .get(&(conn_id, TimerType::Retransmit(seq_nr)))
            .map(|e| e.fire_count)
    }

    /// Check if retransmit timer has exceeded max attempts
    pub fn is_retransmit_max_attempts(&self, conn_id: u16, seq_nr: u16) -> bool {
        self.timers
            .get(&(conn_id, TimerType::Retransmit(seq_nr)))
            .map(|entry| entry.is_max_attempts_reached(TimerType::Retransmit(seq_nr)))
            .unwrap_or(false)
    }
}

mod schedulers;
pub use schedulers::{IdleTimeoutDetector, KeepaliveManager, RetransmitScheduler};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timer_type_display() {
        assert_eq!(TimerType::ConnectTimeout.to_string(), "ConnectTimeout");
        assert_eq!(TimerType::Retransmit(42).to_string(), "Retransmit(seq=42)");
        assert_eq!(TimerType::Keepalive.to_string(), "Keepalive");
        assert_eq!(TimerType::IdleTimeout.to_string(), "IdleTimeout");
    }

    #[test]
    fn test_timer_manager_new() {
        let manager = TimerManager::new();
        assert_eq!(manager.timer_count(), 0);
        assert!(manager.next_timer().is_none());
    }

    #[test]
    fn test_timer_manager_set_timer() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::ConnectTimeout, Duration::from_secs(5));

        assert_eq!(manager.timer_count(), 1);
        assert!(manager.has_timer(1, TimerType::ConnectTimeout));
    }

    #[test]
    fn next_timer_returns_earliest_deadline_not_insertion_order() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::Keepalive, Duration::from_secs(10));
        manager.set_timer(2, TimerType::ConnectTimeout, Duration::from_secs(1));

        let (conn_id, timer_type, remaining) = manager.next_timer().unwrap();
        assert_eq!(conn_id, 2);
        assert_eq!(timer_type, TimerType::ConnectTimeout);
        assert!(remaining <= Duration::from_secs(1));
    }

    #[test]
    fn test_timer_manager_cancel_timer() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::ConnectTimeout, Duration::from_secs(5));
        assert_eq!(manager.timer_count(), 1);

        manager.cancel_timer(1, TimerType::ConnectTimeout);
        assert_eq!(manager.timer_count(), 0);
        assert!(!manager.has_timer(1, TimerType::ConnectTimeout));
    }

    #[test]
    fn test_timer_manager_cancel_all_timers() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::ConnectTimeout, Duration::from_secs(5));
        manager.set_timer(1, TimerType::Keepalive, Duration::from_secs(10));
        manager.set_timer(2, TimerType::IdleTimeout, Duration::from_secs(30));

        assert_eq!(manager.timer_count(), 3);
        assert_eq!(manager.connection_timer_count(1), 2);

        manager.cancel_all_timers(1);
        assert_eq!(manager.timer_count(), 1);
        assert_eq!(manager.connection_timer_count(1), 0);
        assert_eq!(manager.connection_timer_count(2), 1);
    }

    #[test]
    fn test_timer_manager_get_expired_timers() {
        let mut manager = TimerManager::new();

        // Set a short timer and wait long enough to avoid CI jitter
        manager.set_timer(1, TimerType::ConnectTimeout, Duration::from_millis(50));

        // Wait well beyond the expiry to handle scheduler jitter
        std::thread::sleep(Duration::from_millis(200));

        let expired = manager.get_expired_timers();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], (1, TimerType::ConnectTimeout));
        assert_eq!(manager.timer_count(), 0);
    }

    #[test]
    fn test_timer_manager_remaining_time() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::Keepalive, Duration::from_secs(10));

        let remaining = manager.remaining_time(1, TimerType::Keepalive);
        assert!(remaining.is_some());
        let rem = remaining.unwrap();
        assert!(rem <= Duration::from_secs(10));
        assert!(rem > Duration::from_secs(8));
    }

    #[test]
    fn test_timer_manager_clear() {
        let mut manager = TimerManager::new();

        manager.set_timer(1, TimerType::ConnectTimeout, Duration::from_secs(5));
        manager.set_timer(2, TimerType::Keepalive, Duration::from_secs(10));

        manager.clear();
        assert_eq!(manager.timer_count(), 0);
    }

    #[test]
    fn test_timer_manager_defaults() {
        let manager = TimerManager::new();
        assert_eq!(manager.default_connect_timeout(), DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(manager.default_idle_timeout(), DEFAULT_IDLE_TIMEOUT);
        assert_eq!(
            manager.default_keepalive_interval(),
            DEFAULT_KEEPALIVE_INTERVAL
        );
    }

    #[test]
    fn test_retransmit_scheduler_new() {
        let scheduler = RetransmitScheduler::new();
        assert_eq!(scheduler.rto(), DEFAULT_INITIAL_RTO);
        assert_eq!(scheduler.timeout_count(), 0);
        assert!(!scheduler.is_max_attempts_reached());
    }

    #[test]
    fn test_retransmit_scheduler_timeout() {
        let mut scheduler = RetransmitScheduler::new();

        let initial_rto = scheduler.rto();

        scheduler.on_timeout();
        assert_eq!(scheduler.timeout_count(), 1);
        assert!(scheduler.rto() > initial_rto);
    }

    #[test]
    fn test_retransmit_scheduler_ack_received() {
        let mut scheduler = RetransmitScheduler::new();

        scheduler.on_timeout();
        scheduler.on_timeout();
        assert_eq!(scheduler.timeout_count(), 2);

        scheduler.on_ack_received();
        assert_eq!(scheduler.timeout_count(), 0);
        assert_eq!(scheduler.rto(), scheduler.base_rto());
    }

    #[test]
    fn test_keepalive_manager_new() {
        let manager = KeepaliveManager::new();
        assert_eq!(manager.interval(), DEFAULT_KEEPALIVE_INTERVAL);
        assert!(manager.is_enabled());
    }

    #[test]
    fn test_keepalive_manager_should_send() {
        let mut manager = KeepaliveManager::new();

        // Initially should send (no activity)
        assert!(manager.should_send_keepalive());

        // Record activity
        manager.record_activity();
        assert!(!manager.should_send_keepalive());
    }

    #[test]
    fn keepalive_deadline_resets_after_keepalive_and_new_activity() {
        let origin = Instant::now();
        let mut manager = KeepaliveManager::with_interval(Duration::from_secs(10));
        manager.record_activity_at(origin);

        let first_deadline = origin + Duration::from_secs(10);
        assert!(manager.should_send_keepalive_at(first_deadline));
        assert_eq!(
            manager.next_keepalive_at(first_deadline),
            Some(Duration::ZERO)
        );

        manager.record_keepalive_sent_at(first_deadline);
        let after_keepalive = first_deadline + Duration::from_secs(1);
        assert!(!manager.should_send_keepalive_at(after_keepalive));
        assert_eq!(
            manager.next_keepalive_at(after_keepalive),
            Some(Duration::from_secs(9))
        );

        let activity = first_deadline + Duration::from_secs(3);
        manager.record_activity_at(activity);
        let after_activity = first_deadline + Duration::from_secs(5);
        assert!(!manager.should_send_keepalive_at(after_activity));
        assert_eq!(
            manager.next_keepalive_at(after_activity),
            Some(Duration::from_secs(8))
        );
        assert!(manager.should_send_keepalive_at(activity + Duration::from_secs(10)));
    }

    #[test]
    fn test_idle_timeout_detector_new() {
        let detector = IdleTimeoutDetector::new();
        assert_eq!(detector.timeout(), DEFAULT_IDLE_TIMEOUT);
        assert!(detector.is_enabled());
    }

    #[test]
    fn test_idle_timeout_detector_is_timeout() {
        let mut detector = IdleTimeoutDetector::with_timeout(Duration::from_millis(200));

        detector.record_activity();
        assert!(!detector.is_timeout());

        // Sleep well beyond the timeout to avoid scheduler jitter
        std::thread::sleep(Duration::from_millis(500));
        assert!(detector.is_timeout());
    }
}
