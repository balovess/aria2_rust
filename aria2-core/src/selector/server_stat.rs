use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const EMA_ALPHA: f64 = 0.7;

/// Serializable snapshot of ServerStat for persistence.
///
/// This struct contains all the persistent fields of ServerStat in a
/// serde-compatible format (no atomic types). Used for saving and loading
/// server performance statistics across restarts.
///
/// # Example
///
/// ```
/// use aria2_core::selector::server_stat::{ServerStat, ServerStatSnapshot};
///
/// let stat = ServerStat::new("mirror.example.com");
/// stat.update_speed(5000, false);
///
/// let snapshot = stat.to_snapshot();
/// let restored = ServerStat::from_snapshot(&snapshot);
///
/// assert_eq!(restored.host, stat.host);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatSnapshot {
    /// Server hostname (e.g., "mirror1.example.com")
    pub host: String,
    /// Protocol scheme (e.g., "http", "https", "ftp"); empty string if unspecified
    pub protocol: String,
    /// Current download speed in bytes/sec
    pub download_speed: u64,
    /// Exponential moving average of single-connection download speed
    pub single_connection_avg_speed: u64,
    /// Exponential moving average of multi-connection download speed
    pub multi_connection_avg_speed: u64,
    /// Unix timestamp of last update
    pub last_updated: u64,
    /// Server status: 0 = OK, 1 = Error
    pub status: u32,
    /// Usage counter (number of times this server was selected)
    pub counter: u32,
    /// Unix timestamp of last error (None if never failed)
    pub last_error_time: Option<u64>,
    /// HTTP error code of last error (0 if never failed)
    pub last_error_code: u16,
    /// Number of consecutive failures (reset on success)
    pub consecutive_failures: u32,
}

#[derive(Debug)]
pub struct ServerStat {
    /// Server hostname (e.g., "mirror1.example.com")
    pub host: Arc<str>,
    /// Protocol scheme (e.g., "http", "https", "ftp"); empty if unspecified
    ///
    /// In C++ aria2, ServerStat is keyed by (hostname, protocol). This field
    /// enables the same lookup semantics. The Rust ServerStatMan uses
    /// `(host, protocol)` as the composite key.
    pub protocol: Arc<str>,
    download_speed: AtomicU64,
    single_connection_avg_speed: AtomicU64,
    multi_connection_avg_speed: AtomicU64,
    last_updated: AtomicU64,
    status: AtomicU32,
    counter: AtomicU32,
    // Fields for failure tracking and availability cooldown
    // Note: These use plain types (not atomic) because ServerStat is already
    // protected by RwLock in ServerStatMan's internal HashMap
    pub last_error_time: Option<SystemTime>,
    pub last_error_code: Option<u16>,
    pub consecutive_failures: u32,
}

impl Clone for ServerStat {
    fn clone(&self) -> Self {
        Self {
            host: Arc::clone(&self.host),
            protocol: Arc::clone(&self.protocol),
            download_speed: AtomicU64::new(self.download_speed.load(Ordering::Relaxed)),
            single_connection_avg_speed: AtomicU64::new(
                self.single_connection_avg_speed.load(Ordering::Relaxed),
            ),
            multi_connection_avg_speed: AtomicU64::new(
                self.multi_connection_avg_speed.load(Ordering::Relaxed),
            ),
            last_updated: AtomicU64::new(self.last_updated.load(Ordering::Relaxed)),
            status: AtomicU32::new(self.status.load(Ordering::Relaxed)),
            counter: AtomicU32::new(self.counter.load(Ordering::Relaxed)),
            // Clone new fields
            last_error_time: self.last_error_time,
            last_error_code: self.last_error_code,
            consecutive_failures: self.consecutive_failures,
        }
    }
}

impl ServerStat {
    /// Creates a new ServerStat with the given hostname and no protocol.
    ///
    /// This is the backward-compatible constructor. For protocol-aware lookups
    /// (matching C++ aria2 behavior), use [`ServerStat::new_with_protocol`].
    pub fn new(host: &str) -> Self {
        Self {
            host: Arc::from(host),
            protocol: Arc::from(""),
            download_speed: AtomicU64::new(0),
            single_connection_avg_speed: AtomicU64::new(0),
            multi_connection_avg_speed: AtomicU64::new(0),
            last_updated: AtomicU64::new(0),
            status: AtomicU32::new(0),
            counter: AtomicU32::new(0),
            last_error_time: None,
            last_error_code: None,
            consecutive_failures: 0,
        }
    }

    /// Creates a new ServerStat with the given hostname and protocol.
    ///
    /// This matches the C++ aria2 `ServerStat(hostname, protocol)` constructor.
    /// The protocol field enables (host, protocol) composite key lookups.
    pub fn new_with_protocol(host: &str, protocol: &str) -> Self {
        Self::from_shared(Arc::from(host), Arc::from(protocol))
    }

    pub(crate) fn from_shared(host: Arc<str>, protocol: Arc<str>) -> Self {
        Self {
            host,
            protocol,
            download_speed: AtomicU64::new(0),
            single_connection_avg_speed: AtomicU64::new(0),
            multi_connection_avg_speed: AtomicU64::new(0),
            last_updated: AtomicU64::new(0),
            status: AtomicU32::new(0),
            counter: AtomicU32::new(0),
            // Initialize new fields
            last_error_time: None,
            last_error_code: None,
            consecutive_failures: 0,
        }
    }

    pub fn update_speed(&self, speed: u64, is_multi: bool) {
        self.download_speed.store(speed, Ordering::Relaxed);
        if is_multi {
            let old = self.multi_connection_avg_speed.load(Ordering::Relaxed);
            let new_val = ema(old, speed);
            self.multi_connection_avg_speed
                .store(new_val, Ordering::Relaxed);
        } else {
            let old = self.single_connection_avg_speed.load(Ordering::Relaxed);
            let new_val = ema(old, speed);
            self.single_connection_avg_speed
                .store(new_val, Ordering::Relaxed);
        }
        self.touch();
    }

    pub fn get_download_speed(&self) -> u64 {
        self.download_speed.load(Ordering::Relaxed)
    }

    pub fn get_single_avg_speed(&self) -> u64 {
        self.single_connection_avg_speed.load(Ordering::Relaxed)
    }

    pub fn get_multi_avg_speed(&self) -> u64 {
        self.multi_connection_avg_speed.load(Ordering::Relaxed)
    }

    pub fn get_avg_speed(&self) -> u64 {
        let s = self.single_connection_avg_speed.load(Ordering::Relaxed);
        let m = self.multi_connection_avg_speed.load(Ordering::Relaxed);
        if s > 0 && m > 0 {
            (s + m) / 2
        } else {
            s.max(m)
        }
    }

    pub fn is_ok(&self) -> bool {
        self.status.load(Ordering::Relaxed) == 0
    }

    pub fn set_error(&self) {
        self.status.store(1, Ordering::Relaxed);
    }

    pub fn reset_status(&self) {
        self.status.store(0, Ordering::Relaxed);
    }

    pub fn increment_counter(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    pub fn get_counter(&self) -> u32 {
        self.counter.load(Ordering::Relaxed)
    }

    pub fn reset_counter(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }

    pub fn is_fresh(&self, duration_secs: u64) -> bool {
        let last = self.last_updated.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(last) < duration_secs
    }

    /// Get the last update timestamp as unix timestamp (0 if never updated).
    pub fn get_last_updated(&self) -> u64 {
        self.last_updated.load(Ordering::Relaxed)
    }

    fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_updated.store(now, Ordering::Relaxed);
    }

    /// Check if server is available (not in cooldown due to consecutive failures)
    pub fn is_available(&self) -> bool {
        if self.consecutive_failures >= 3
            && let Some(error_time) = self.last_error_time
            && let Ok(elapsed) = error_time.elapsed()
        {
            return elapsed.as_secs() > 60; // cooldown expired?
        }
        true
    }

    /// Get the last error time as unix timestamp (0 if never failed)
    pub fn get_last_error_time(&self) -> u64 {
        self.last_error_time
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(0)
    }

    /// Get the last error code (0 if never failed)
    pub fn get_last_error_code(&self) -> u16 {
        self.last_error_code.unwrap_or(0)
    }

    /// Get consecutive failure count
    pub fn get_consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Set error tracking fields (for use by ServerStatMan::mark_failure)
    /// Note: Requires &mut self because these fields are not atomic
    pub fn set_failure_info(&mut self, error_code: u16) {
        self.last_error_time = Some(SystemTime::now());
        self.last_error_code = Some(error_code);
        self.consecutive_failures += 1;
    }

    // ==================== Persistence Methods ====================

    /// Convert to a serializable snapshot for persistence.
    ///
    /// Extracts all atomic values into a plain struct suitable for
    /// JSON serialization. The snapshot captures the current state
    /// of all performance metrics and error tracking.
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::selector::server_stat::ServerStat;
    ///
    /// let stat = ServerStat::new("fast.mirror.com");
    /// stat.update_speed(10000, false);
    /// stat.increment_counter();
    ///
    /// let snapshot = stat.to_snapshot();
    /// assert_eq!(snapshot.host, "fast.mirror.com");
    /// assert_eq!(snapshot.counter, 1);
    /// ```
    pub fn to_snapshot(&self) -> ServerStatSnapshot {
        ServerStatSnapshot {
            host: self.host.to_string(),
            protocol: self.protocol.to_string(),
            download_speed: self.download_speed.load(Ordering::Relaxed),
            single_connection_avg_speed: self.single_connection_avg_speed.load(Ordering::Relaxed),
            multi_connection_avg_speed: self.multi_connection_avg_speed.load(Ordering::Relaxed),
            last_updated: self.last_updated.load(Ordering::Relaxed),
            status: self.status.load(Ordering::Relaxed),
            counter: self.counter.load(Ordering::Relaxed),
            last_error_time: self
                .last_error_time
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())),
            last_error_code: self.last_error_code.unwrap_or(0),
            consecutive_failures: self.consecutive_failures,
        }
    }

    /// Restore from a serialized snapshot.
    ///
    /// Creates a new ServerStat instance with all fields initialized
    /// from the snapshot data. Atomic fields are set to the snapshot values.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The serialized server statistics to restore from
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::selector::server_stat::{ServerStat, ServerStatSnapshot};
    ///
    /// let snapshot = ServerStatSnapshot {
    ///     host: "restored.mirror.com".to_string(),
    ///     protocol: "https".to_string(),
    ///     download_speed: 5000,
    ///     single_connection_avg_speed: 4500,
    ///     multi_connection_avg_speed: 8000,
    ///     last_updated: 1700000000,
    ///     status: 0,
    ///     counter: 10,
    ///     last_error_time: None,
    ///     last_error_code: 0,
    ///     consecutive_failures: 0,
    /// };
    ///
    /// let stat = ServerStat::from_snapshot(&snapshot);
    /// assert_eq!(stat.host.as_ref(), "restored.mirror.com");
    /// assert_eq!(stat.get_counter(), 10);
    /// ```
    pub fn from_snapshot(snapshot: &ServerStatSnapshot) -> Self {
        Self::from_snapshot_shared(
            snapshot,
            Arc::from(snapshot.host.as_str()),
            Arc::from(snapshot.protocol.as_str()),
        )
    }

    pub(crate) fn from_snapshot_shared(
        snapshot: &ServerStatSnapshot,
        host: Arc<str>,
        protocol: Arc<str>,
    ) -> Self {
        Self {
            host,
            protocol,
            download_speed: AtomicU64::new(snapshot.download_speed),
            single_connection_avg_speed: AtomicU64::new(snapshot.single_connection_avg_speed),
            multi_connection_avg_speed: AtomicU64::new(snapshot.multi_connection_avg_speed),
            last_updated: AtomicU64::new(snapshot.last_updated),
            status: AtomicU32::new(snapshot.status),
            counter: AtomicU32::new(snapshot.counter),
            last_error_time: snapshot
                .last_error_time
                .and_then(|ts| UNIX_EPOCH.checked_add(std::time::Duration::from_secs(ts))),
            last_error_code: if snapshot.last_error_code > 0 {
                Some(snapshot.last_error_code)
            } else {
                None
            },
            consecutive_failures: snapshot.consecutive_failures,
        }
    }
}

fn ema(old: u64, new: u64) -> u64 {
    (old as f64 * (1.0 - EMA_ALPHA) + new as f64 * EMA_ALPHA) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("server_stat_tests.rs");
}
