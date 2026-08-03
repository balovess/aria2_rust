//! Per-download network statistics: byte counters and speed tracking.

use std::time::{Duration, Instant};

/// Per-download network statistics.
///
/// Tracks download/upload byte counters and speed. The speed fields are
/// updated externally (e.g. by the download engine's rolling-window
/// calculator); the counters are incremented via [`DownloadContext::update_download`]
/// and [`DownloadContext::update_upload_length`].
#[derive(Debug, Default)]
pub struct NetStat {
    /// Cumulative bytes downloaded in the current session.
    session_download_length: u64,
    /// Cumulative bytes uploaded in the current session.
    session_upload_length: u64,
    /// Current download speed (bytes/sec), updated externally.
    download_speed: u64,
    /// Current upload speed (bytes/sec), updated externally.
    upload_speed: u64,
    /// Monotonic timestamp when the download started.
    download_start_time: Option<Instant>,
    /// Monotonic timestamp when the download stopped.
    download_stop_time: Option<Instant>,
}

impl NetStat {
    /// Mark the download as started — records the current time.
    pub fn download_start(&mut self) {
        self.download_start_time = Some(Instant::now());
    }

    /// Mark the download as stopped — records the current time.
    pub fn download_stop(&mut self) {
        self.download_stop_time = Some(Instant::now());
    }

    /// Add `bytes` to the session download counter.
    pub fn update_download(&mut self, bytes: u64) {
        self.session_download_length += bytes;
    }

    /// Add `bytes` to the session upload counter.
    pub fn update_upload_length(&mut self, bytes: u64) {
        self.session_upload_length += bytes;
    }

    /// Set the upload speed (bytes/sec).
    pub fn update_upload_speed(&mut self, bytes: u64) {
        self.upload_speed = bytes;
    }

    /// Return the session download length.
    pub fn session_download_length(&self) -> u64 {
        self.session_download_length
    }

    /// Return the session upload length.
    pub fn session_upload_length(&self) -> u64 {
        self.session_upload_length
    }

    /// Return the current download speed.
    pub fn download_speed(&self) -> u64 {
        self.download_speed
    }

    /// Set the current download speed.
    pub fn set_download_speed(&mut self, speed: u64) {
        self.download_speed = speed;
    }

    /// Return the current upload speed.
    pub fn upload_speed(&self) -> u64 {
        self.upload_speed
    }

    /// Return the recorded download start time.
    pub fn download_start_time(&self) -> Option<Instant> {
        self.download_start_time
    }

    /// Return the recorded download stop time.
    pub fn download_stop_time(&self) -> Option<Instant> {
        self.download_stop_time
    }

    /// Calculate the session duration.
    ///
    /// Returns the elapsed time between `download_start_time` and
    /// `download_stop_time`. If either is missing, returns `Duration::ZERO`.
    pub fn calculate_session_time(&self) -> Duration {
        match (self.download_start_time, self.download_stop_time) {
            (Some(start), Some(stop)) => stop.duration_since(start),
            _ => Duration::ZERO,
        }
    }
}
