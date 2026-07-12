use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::error::Result;
use crate::rate_limiter::RateLimiter;
use crate::segment::Segment;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DownloadStatus {
    #[default]
    Waiting,
    Active,
    Paused,
    Error(String),
    Complete,
    Removed,
}

// Custom serde impl so every variant serializes as a plain lowercase string
// (e.g. "active", "waiting", "error"), matching C++ aria2's wire format that
// browser plugins expect. The `Error(String)` payload (error message) is not
// emitted here; callers surface it via `StatusInfo.error_message` instead.
impl Serialize for DownloadStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DownloadStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "waiting" => Ok(DownloadStatus::Waiting),
            "active" => Ok(DownloadStatus::Active),
            "paused" => Ok(DownloadStatus::Paused),
            "error" => Ok(DownloadStatus::Error(String::new())),
            "complete" => Ok(DownloadStatus::Complete),
            "removed" => Ok(DownloadStatus::Removed),
            _ => Err(serde::de::Error::unknown_variant(
                "DownloadStatus",
                &["waiting", "active", "paused", "error", "complete", "removed"],
            )),
        }
    }
}

impl fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadStatus::Waiting => write!(f, "waiting"),
            DownloadStatus::Active => write!(f, "active"),
            DownloadStatus::Paused => write!(f, "paused"),
            DownloadStatus::Error(_) => write!(f, "error"),
            DownloadStatus::Complete => write!(f, "complete"),
            DownloadStatus::Removed => write!(f, "removed"),
        }
    }
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, DownloadStatus::Active | DownloadStatus::Waiting)
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, DownloadStatus::Complete)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, DownloadStatus::Paused)
    }

    pub fn is_stopped(&self) -> bool {
        !self.is_active() && !matches!(self, DownloadStatus::Removed)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadStatus::Active => "active",
            DownloadStatus::Waiting => "waiting",
            DownloadStatus::Paused => "paused",
            DownloadStatus::Error(_) => "error",
            DownloadStatus::Complete => "complete",
            DownloadStatus::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u64);

impl GroupId {
    pub fn new(id: u64) -> Self {
        GroupId(id)
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    /// Create GroupId from hex string (e.g., "deadbeef")
    ///
    /// Returns None if the string is not valid hex or too large for u64.
    pub fn from_hex_string(hex_str: &str) -> Option<Self> {
        let trimmed = hex_str.trim_start_matches("0x");
        if trimmed.is_empty() {
            return None;
        }
        let val = u64::from_str_radix(trimmed, 16).ok()?;
        Some(GroupId(val))
    }

    /// Generate a random GroupId using current timestamp + random
    pub fn new_random() -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut hasher = DefaultHasher::new();
        nanos.hash(&mut hasher);
        rand::random::<u64>().hash(&mut hasher);
        GroupId(hasher.finish())
    }

    /// Format GID as hex string (lowercase, no prefix)
    pub fn to_hex_string(&self) -> String {
        format!("{:016x}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub split: Option<u16>,
    pub max_connection_per_server: Option<u16>,
    pub max_download_limit: Option<u64>,
    pub max_upload_limit: Option<u64>,
    pub dir: Option<String>,
    pub out: Option<String>,
    /// File allocation strategy: "none", "prealloc", "falloc", "trunc", or "mmap".
    /// When "mmap", `MmapDiskWriter` is used for files above `mmap_threshold`.
    pub file_allocation: Option<String>,
    /// File size threshold (bytes) above which mmap writes are used when
    /// `file_allocation = "mmap"`. Default: 256 MiB.
    pub mmap_threshold: Option<u64>,
    /// Zero-fill allocated blocks after fallocate on platforms that don't
    /// zero-fill (macOS `F_PREALLOCATE`, Windows `SetFileValidData`).
    /// Prevents exposure of residual disk data at a performance cost.
    /// Has no effect on Linux. Defaults to `false` (matches
    /// `constants::DEFAULT_SECURE_FALLOC`).
    pub secure_falloc: bool,
    pub seed_time: Option<u64>,
    pub seed_ratio: Option<f64>,
    pub checksum: Option<(String, String)>,
    pub cookie_file: Option<String>,
    pub cookies: Option<String>,
    pub bt_force_encrypt: bool,
    pub bt_require_crypto: bool,
    pub enable_dht: bool,
    pub dht_listen_port: Option<u16>,
    pub dht_entry_point: Option<Vec<String>>,
    pub enable_public_trackers: bool,
    pub bt_piece_selection_strategy: String,
    pub bt_endgame_threshold: u32,
    pub max_retries: u32,
    pub retry_wait: u64,
    pub http_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub ftp_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub dht_file_path: Option<String>,

    // ------------------------------------------------------------------
    // Choking algorithm configuration (BT tit-for-tat)
    // ------------------------------------------------------------------
    /// Maximum number of peers to unchoke simultaneously during seeding.
    /// Default: 4. Set to enable the choking algorithm.
    pub bt_max_upload_slots: Option<u32>,

    /// Interval in seconds between optimistic unchokes.
    /// Default: 30.
    pub bt_optimistic_unchoke_interval: Option<u64>,

    /// Timeout in seconds after which a peer is considered snubbed (not sending data).
    /// Default: 60.
    pub bt_snubbed_timeout: Option<u64>,

    // ------------------------------------------------------------------
    // Piece selection priority mode (G2)
    // ------------------------------------------------------------------
    /// Piece selection priority: "rarest" (default), "head" (sequential from start),
    /// "tail" (sequential from end).
    pub bt_prioritize_piece: String,

    // ------------------------------------------------------------------
    // uTP (UDP Transport Protocol - BEP 29)
    // ------------------------------------------------------------------
    /// Enable uTP (UDP Transport Protocol) for BitTorrent connections.
    /// This implements BEP 29 and is an experimental feature not in original aria2.
    /// Default: false.
    pub enable_utp: bool,

    /// UDP port for uTP connections. 0 = auto-assign.
    /// Experimental feature not in original aria2.
    pub utp_listen_port: Option<u16>,

    // ------------------------------------------------------------------
    // HTTP headers (C++ aria2 `--header` / RPC `header` option)
    // ------------------------------------------------------------------
    /// Custom HTTP request headers as `"Name: Value"` strings.
    /// Applied to both HEAD probes and range GETs.
    pub header: Vec<String>,
    /// Override `User-Agent` header. Also injected into the `header` list by
    /// [`DownloadOptions::parsed_headers`] when set.
    pub user_agent: Option<String>,
    /// Override `Referer` header. Also injected into the `header` list by
    /// [`DownloadOptions::parsed_headers`] when set.
    pub referer: Option<String>,
}

// Manual Default impl: `enable_dht` and `enable_public_trackers` default to
// `true` (matching the load path in `option_handler.rs` and `task.rs` which
// use `unwrap_or(true)`). All other fields use their type-level defaults.
impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            split: None,
            max_connection_per_server: None,
            max_download_limit: None,
            max_upload_limit: None,
            dir: None,
            out: None,
            file_allocation: None,
            mmap_threshold: None,
            secure_falloc: false,
            seed_time: None,
            seed_ratio: None,
            checksum: None,
            cookie_file: None,
            cookies: None,
            bt_force_encrypt: false,
            bt_require_crypto: false,
            enable_dht: true,
            dht_listen_port: None,
            dht_entry_point: None,
            enable_public_trackers: true,
            bt_piece_selection_strategy: String::new(),
            bt_endgame_threshold: 0,
            max_retries: 0,
            retry_wait: 0,
            http_proxy: None,
            all_proxy: None,
            https_proxy: None,
            ftp_proxy: None,
            no_proxy: None,
            dht_file_path: None,
            bt_max_upload_slots: None,
            bt_optimistic_unchoke_interval: None,
            bt_snubbed_timeout: None,
            bt_prioritize_piece: String::new(),
            enable_utp: false,
            utp_listen_port: None,
            header: Vec::new(),
            user_agent: None,
            referer: None,
        }
    }
}

impl DownloadOptions {
    /// Parse the raw `header` list into `(name, value)` pairs, splitting each
    /// `"Name: Value"` entry on the first `:`. When `user_agent` or `referer`
    /// are set, they are appended as `User-Agent` / `Referer` pairs (unless an
    /// entry with the same name already exists), so callers only need to handle
    /// a single header list.
    pub fn parsed_headers(&self) -> Vec<(String, String)> {
        let mut result: Vec<(String, String)> = Vec::new();
        for raw in &self.header {
            if let Some((name, value)) = raw.split_once(':') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if !name.is_empty() {
                    result.push((name, value));
                }
            }
        }
        // Overlay user_agent / referer if not already present (case-insensitive).
        if let Some(ref ua) = self.user_agent
            && !has_header(&result, "User-Agent")
        {
            result.push(("User-Agent".to_string(), ua.clone()));
        }
        if let Some(ref ref_) = self.referer
            && !has_header(&result, "Referer")
        {
            result.push(("Referer".to_string(), ref_.clone()));
        }
        result
    }
}

/// Case-insensitive check whether a `(name, value)` header list already contains
/// an entry with the given name.
fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// Options that can be changed at runtime via `aria2.changeOption`.
/// All other options are startup-only and cannot be modified after the
/// download begins. Keep in sync with [`RequestGroup::update_option`].
pub const RUNTIME_CHANGEABLE_OPTIONS: &[&str] = &[
    "max-download-limit",
    "max-upload-limit",
    "max-retries",
    "retry-wait",
    "header",
    "user-agent",
    "referer",
];

pub struct RequestGroup {
    gid: GroupId,
    uris: Vec<String>,
    options: DownloadOptions,
    status: Arc<RwLock<DownloadStatus>>,
    segments: Arc<RwLock<Vec<Segment>>>,
    total_length: u64,
    completed_length: Arc<RwLock<u64>>,
    download_speed: Arc<RwLock<u64>>,
    upload_speed: Arc<RwLock<u64>>,
    start_time: Arc<RwLock<Option<std::time::Instant>>>,
    end_time: Arc<RwLock<Option<std::time::Instant>>>,
    // New progress tracking fields (for session persistence)
    pub completed_length_atomic: AtomicU64,
    pub total_length_atomic: AtomicU64,
    pub uploaded_length: AtomicU64,
    pub download_speed_cached: AtomicU64,
    pub bt_bitfield: RwLock<Option<Vec<u8>>>,
    
    // BT metadata fields (for session persistence enhancement)
    /// Number of pieces in the torrent (0 for non-BT downloads)
    pub bt_num_pieces: AtomicU32,
    /// Size of each piece in bytes (0 for non-BT downloads)
    pub bt_piece_length: AtomicU32,
    /// Info hash hex string for torrent identification (None for non-BT downloads)
    /// Uses std::sync::RwLock instead of tokio::sync::RwLock for non-async access
    pub bt_info_hash_hex: std::sync::RwLock<Option<String>>,

    /// Handle to the download's `RateLimiter` so that runtime option updates
    /// (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    /// `None` until the download engine wires up a `ThrottledWriter`.
    /// Uses its own `RwLock` (independent of the `RequestGroupMan` write lock)
    /// so that `set_rate_limiter` can take `&self`.
    pub rate_limiter: RwLock<Option<RateLimiter>>,
}

impl RequestGroup {
    pub fn new(gid: GroupId, uris: Vec<String>, options: DownloadOptions) -> Self {
        info!("Creating request group #{}", gid.value());

        RequestGroup {
            gid,
            uris,
            options,
            status: Arc::new(RwLock::new(DownloadStatus::Waiting)),
            segments: Arc::new(RwLock::new(Vec::new())),
            total_length: 0,
            completed_length: Arc::new(RwLock::new(0)),
            download_speed: Arc::new(RwLock::new(0)),
            upload_speed: Arc::new(RwLock::new(0)),
            start_time: Arc::new(RwLock::new(None)),
            end_time: Arc::new(RwLock::new(None)),
            // Initialize new progress tracking fields
            completed_length_atomic: AtomicU64::new(0),
            total_length_atomic: AtomicU64::new(0),
            uploaded_length: AtomicU64::new(0),
            download_speed_cached: AtomicU64::new(0),
            bt_bitfield: RwLock::new(None),
            
            // Initialize BT metadata fields (default to 0/None for non-BT downloads)
            bt_num_pieces: AtomicU32::new(0),
            bt_piece_length: AtomicU32::new(0),
            bt_info_hash_hex: std::sync::RwLock::new(None),

            // Rate limiter is wired up later by the download engine via
            // `set_rate_limiter` once a `ThrottledWriter` is constructed.
            rate_limiter: RwLock::new(None),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut start_time = self.start_time.write().await;

        *status = DownloadStatus::Active;
        *start_time = Some(std::time::Instant::now());

        info!("Starting download task #{}", self.gid.value());
        Ok(())
    }

    pub async fn pause(&mut self) -> Result<()> {
        let mut status = self.status.write().await;

        if matches!(*status, DownloadStatus::Active) {
            *status = DownloadStatus::Paused;
            info!("Pausing download task #{}", self.gid.value());
        }

        Ok(())
    }

    pub async fn remove(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;

        *status = DownloadStatus::Removed;
        *end_time = Some(std::time::Instant::now());

        info!("Removing download task #{}", self.gid.value());
        Ok(())
    }

    pub async fn complete(&mut self) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;
        let mut completed_length = self.completed_length.write().await;

        *status = DownloadStatus::Complete;
        *end_time = Some(std::time::Instant::now());
        *completed_length = self.total_length;

        info!("Completing download task #{}", self.gid.value());
        Ok(())
    }

    pub async fn error(&mut self, err: impl Into<String>) -> Result<()> {
        let mut status = self.status.write().await;
        let mut end_time = self.end_time.write().await;

        *status = DownloadStatus::Error(err.into());
        *end_time = Some(std::time::Instant::now());

        debug!("Download task #{} encountered error", self.gid.value());
        Ok(())
    }

    pub async fn status(&self) -> DownloadStatus {
        self.status.read().await.clone()
    }

    pub fn gid(&self) -> GroupId {
        self.gid
    }

    pub fn uris(&self) -> &[String] {
        &self.uris
    }

    pub fn options(&self) -> &DownloadOptions {
        &self.options
    }

    /// Store a handle to the download's `RateLimiter` so that runtime option
    /// updates (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    /// The `RateLimiter` is `Clone` and shares `Arc<RateLimiterInner>`, so both
    /// the `ThrottledWriter` and `RequestGroup` see the same token buckets.
    ///
    /// Takes `&self` because `rate_limiter` has its own `RwLock`, independent
    /// of the `RequestGroupMan` outer write lock.
    pub async fn set_rate_limiter(&self, limiter: RateLimiter) {
        *self.rate_limiter.write().await = Some(limiter);
    }

    /// Update a single runtime-changeable option by key (using aria2's
    /// kebab-case option names, e.g. `"max-download-limit"`).
    ///
    /// Returns `true` if the option was recognized and updated, `false` if the
    /// key is not a runtime-changeable option (see [`RUNTIME_CHANGEABLE_OPTIONS`]).
    ///
    /// Takes `&mut self` because `options` is a plain field — the caller
    /// (`RequestGroupMan`) holds the outer `Arc<RwLock<RequestGroup>>` write
    /// lock. For `max-download-limit` / `max-upload-limit`, the stored
    /// `RateLimiter` (if any) is also updated so the change takes effect
    /// immediately on the live download.
    pub async fn update_option(&mut self, key: &str, value: serde_json::Value) -> bool {
        match key {
            "max-download-limit" => {
                let rate = value.as_u64();
                self.options.max_download_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.read().await {
                    limiter.set_download_rate(rate);
                }
                true
            }
            "max-upload-limit" => {
                let rate = value.as_u64();
                self.options.max_upload_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.read().await {
                    limiter.set_upload_rate(rate);
                }
                true
            }
            "max-retries" => {
                if let Some(v) = value.as_u64() {
                    self.options.max_retries = v as u32;
                }
                true
            }
            "retry-wait" => {
                if let Some(v) = value.as_u64() {
                    self.options.retry_wait = v;
                }
                true
            }
            "header" => {
                // Accept both a JSON array of strings and a newline-separated
                // string (matching aria2's wire format).
                match &value {
                    serde_json::Value::Array(arr) => {
                        self.options.header = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect();
                    }
                    serde_json::Value::String(s) => {
                        self.options.header = s
                            .split('\n')
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect();
                    }
                    _ => {}
                }
                true
            }
            "user-agent" => {
                self.options.user_agent = value.as_str().map(|s| s.to_string());
                true
            }
            "referer" => {
                self.options.referer = value.as_str().map(|s| s.to_string());
                true
            }
            _ => false,
        }
    }

    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    pub async fn set_total_length(&mut self, length: u64) {
        self.total_length = length;
        debug!("Setting total length: {} bytes", length);
    }

    pub async fn completed_length(&self) -> u64 {
        *self.completed_length.read().await
    }

    pub async fn update_completed_length(&self, length: u64) {
        let mut completed_length = self.completed_length.write().await;
        *completed_length = length;
    }

    pub async fn update_progress(&self, completed_length: u64) {
        let mut cl = self.completed_length.write().await;
        *cl = completed_length;
    }

    pub async fn progress(&self) -> f64 {
        let total = self.total_length;
        let completed = *self.completed_length.read().await;

        if total == 0 {
            0.0
        } else {
            (completed as f64 / total as f64) * 100.0
        }
    }

    pub async fn download_speed(&self) -> u64 {
        *self.download_speed.read().await
    }

    pub async fn upload_speed(&self) -> u64 {
        *self.upload_speed.read().await
    }

    pub async fn update_speed(&self, dl_speed: u64, ul_speed: u64) {
        let mut ds = self.download_speed.write().await;
        let mut us = self.upload_speed.write().await;
        *ds = dl_speed;
        *us = ul_speed;
    }

    pub async fn add_segment(&mut self, segment: Segment) {
        let mut segments = self.segments.write().await;
        segments.push(segment);
        debug!("Adding segment, current segments: {}", segments.len());
    }

    pub async fn segments(&self) -> Vec<Segment> {
        self.segments.read().await.clone()
    }

    pub async fn elapsed_time(&self) -> Option<std::time::Duration> {
        let start = *self.start_time.read().await;
        start.map(|t| t.elapsed())
    }

    pub async fn eta(&self) -> Option<std::time::Duration> {
        let speed = *self.download_speed.read().await;
        let remaining = self
            .total_length
            .saturating_sub(*self.completed_length.read().await);

        if speed == 0 || remaining == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(remaining / speed))
        }
    }

    // New progress tracking methods (for session persistence)
    // These use AtomicU64 for lock-free reads, suitable for frequent polling

    /// Set completed length using atomic store (lock-free)
    pub fn set_completed_length(&self, val: u64) {
        self.completed_length_atomic.store(val, Ordering::Relaxed);
    }

    /// Get completed length using atomic load (lock-free)
    pub fn get_completed_length(&self) -> u64 {
        self.completed_length_atomic.load(Ordering::Relaxed)
    }

    /// Set total length using atomic store (lock-free)
    pub fn set_total_length_atomic(&self, val: u64) {
        self.total_length_atomic.store(val, Ordering::Relaxed);
    }

    /// Get total length using atomic load (lock-free)
    pub fn get_total_length_atomic(&self) -> u64 {
        self.total_length_atomic.load(Ordering::Relaxed)
    }

    /// Set uploaded length using atomic store (lock-free)
    pub fn set_uploaded_length(&self, val: u64) {
        self.uploaded_length.store(val, Ordering::Relaxed);
    }

    /// Get uploaded length using atomic load (lock-free)
    pub fn get_uploaded_length(&self) -> u64 {
        self.uploaded_length.load(Ordering::Relaxed)
    }

    /// Set download speed cache using atomic store (lock-free)
    pub fn set_download_speed_cached(&self, val: u64) {
        self.download_speed_cached.store(val, Ordering::Relaxed);
    }

    /// Get download speed from cache using atomic load (lock-free)
    pub fn get_download_speed_cached(&self) -> u64 {
        self.download_speed_cached.load(Ordering::Relaxed)
    }

    /// Set BT bitfield (async, uses RwLock)
    pub async fn set_bt_bitfield(&self, bf: Option<Vec<u8>>) {
        *self.bt_bitfield.write().await = bf;
    }

    /// Get BT bitfield (async, uses RwLock)
    pub async fn get_bt_bitfield(&self) -> Option<Vec<u8>> {
        self.bt_bitfield.read().await.clone()
    }

    /// Set resume offset for HTTP/FTP range request resumption
    pub fn set_resume_offset(&self, offset: u64) {
        // Store resume offset in completed_length so the download engine
        // knows where to resume from
        self.completed_length_atomic
            .store(offset, Ordering::Relaxed);
    }
    
    // BT metadata methods (for session persistence enhancement)
    
    /// Set BT metadata fields (num_pieces, piece_length, info_hash_hex)
    /// Called by BtDownloadCommand after parsing TorrentMeta
    pub fn set_bt_metadata(&self, num_pieces: u32, piece_length: u32, info_hash_hex: String) {
        self.bt_num_pieces.store(num_pieces, Ordering::Relaxed);
        self.bt_piece_length.store(piece_length, Ordering::Relaxed);
        // Use std::sync::RwLock for non-async access
        *self.bt_info_hash_hex.write().unwrap() = Some(info_hash_hex);
    }
    
    /// Get number of pieces (lock-free atomic read)
    pub fn get_bt_num_pieces(&self) -> u32 {
        self.bt_num_pieces.load(Ordering::Relaxed)
    }
    
    /// Get piece length (lock-free atomic read)
    pub fn get_bt_piece_length(&self) -> u32 {
        self.bt_piece_length.load(Ordering::Relaxed)
    }
    
    /// Get info hash hex string (async-compatible wrapper)
    pub async fn get_bt_info_hash_hex_async(&self) -> Option<String> {
        // std::sync::RwLock can be used in async context for short reads
        self.bt_info_hash_hex.read().unwrap().clone()
    }
    
    /// Get info hash hex string (blocking read for non-async contexts)
    pub fn get_bt_info_hash_hex(&self) -> Option<String> {
        self.bt_info_hash_hex.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_group_progress_fields_default() {
        // New RequestGroup should have all zeros/None defaults for progress fields
        let group = RequestGroup::new(
            GroupId::new(1),
            vec!["http://example.com/file.zip".to_string()],
            DownloadOptions::default(),
        );

        // Verify all atomic fields default to 0
        assert_eq!(
            group.get_completed_length(),
            0,
            "completed_length_atomic should default to 0"
        );
        assert_eq!(
            group.get_total_length_atomic(),
            0,
            "total_length_atomic should default to 0"
        );
        assert_eq!(
            group.get_uploaded_length(),
            0,
            "uploaded_length should default to 0"
        );
        assert_eq!(
            group.get_download_speed_cached(),
            0,
            "download_speed_cached should default to 0"
        );
    }

    #[test]
    fn test_set_get_completed_length() {
        let group = RequestGroup::new(
            GroupId::new(2),
            vec!["http://test.com/file.bin".to_string()],
            DownloadOptions::default(),
        );

        // Test set/get roundtrip
        group.set_completed_length(1024);
        assert_eq!(
            group.get_completed_length(),
            1024,
            "Should return 1024 after setting"
        );

        // Test update to different value
        group.set_completed_length(2048);
        assert_eq!(
            group.get_completed_length(),
            2048,
            "Should return 2048 after update"
        );

        // Test large value
        group.set_completed_length(u64::MAX);
        assert_eq!(
            group.get_completed_length(),
            u64::MAX,
            "Should handle u64::MAX"
        );

        // Test zero
        group.set_completed_length(0);
        assert_eq!(group.get_completed_length(), 0, "Should handle 0");
    }

    #[test]
    fn test_set_get_total_length() {
        let group = RequestGroup::new(
            GroupId::new(3),
            vec!["http://example.com/large.iso".to_string()],
            DownloadOptions::default(),
        );

        // Test set/get roundtrip
        group.set_total_length_atomic(1048576); // 1MB
        assert_eq!(
            group.get_total_length_atomic(),
            1048576,
            "Should return 1MB after setting"
        );

        // Test update
        group.set_total_length_atomic(1073741824); // 1GB
        assert_eq!(
            group.get_total_length_atomic(),
            1073741824,
            "Should return 1GB after update"
        );
    }

    #[tokio::test]
    async fn test_set_get_bt_bitfield() {
        let group = RequestGroup::new(
            GroupId::new(4),
            vec!["magnet:?xt=urn:btih:abc123".to_string()],
            DownloadOptions::default(),
        );

        // Default should be None
        let bf = group.get_bt_bitfield().await;
        assert!(bf.is_none(), "bt_bitfield should default to None");

        // Set and retrieve bitfield
        let test_bitfield = vec![0xFF, 0xF0, 0x0F];
        group.set_bt_bitfield(Some(test_bitfield.clone())).await;
        let retrieved = group.get_bt_bitfield().await;
        assert!(
            retrieved.is_some(),
            "bt_bitfield should be Some after setting"
        );
        assert_eq!(
            retrieved.unwrap(),
            test_bitfield,
            "bitfield should match what was set"
        );

        // Set back to None
        group.set_bt_bitfield(None).await;
        let bf_none = group.get_bt_bitfield().await;
        assert!(
            bf_none.is_none(),
            "bt_bitfield should be None after clearing"
        );

        // Test with empty bitfield
        group.set_bt_bitfield(Some(vec![])).await;
        let empty_bf = group.get_bt_bitfield().await;
        assert!(empty_bf.is_some(), "empty bitfield should still be Some");
        assert!(empty_bf.unwrap().is_empty(), "bitfield should be empty vec");
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        use std::sync::Arc;

        let group = Arc::new(RequestGroup::new(
            GroupId::new(5),
            vec!["http://load.test/file.dat".to_string()],
            DownloadOptions::default(),
        ));

        // Spawn multiple tasks that read/write progress concurrently
        let mut handles = Vec::new();

        for i in 0..10 {
            let g = Arc::clone(&group);
            handles.push(tokio::spawn(async move {
                // Write progress
                g.set_completed_length(i * 100);
                g.set_total_length_atomic(10000);
                g.set_uploaded_length(i * 10);
                g.set_download_speed_cached(i * 1000);

                // Read progress (should not deadlock)
                let _cl = g.get_completed_length();
                let _tl = g.get_total_length_atomic();
                let _ul = g.get_uploaded_length();
                let _ds = g.get_download_speed_cached();

                // Occasionally write bitfield (async)
                if i % 3 == 0 {
                    let bf = vec![i as u8; 8];
                    g.set_bt_bitfield(Some(bf)).await;
                    let _retrieved = g.get_bt_bitfield().await;
                }

                // Small delay to increase chance of race conditions
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }));
        }

        // Wait for all tasks to complete without deadlock
        for handle in handles {
            handle.await.expect("Task should complete without panic");
        }

        // Verify final state is consistent
        let final_cl = group.get_completed_length();
        let final_tl = group.get_total_length_atomic();
        let final_ul = group.get_uploaded_length();
        let final_ds = group.get_download_speed_cached();

        // Values should be from one of the concurrent writers (we don't know which)
        assert!(final_cl <= 900, "completed_length should be <= 900");
        assert_eq!(final_tl, 10000, "total_length should be 10000");
        assert!(final_ul <= 90, "uploaded_length should be <= 90");
        assert!(final_ds <= 9000, "download_speed should be <= 9000");
    }

    #[test]
    fn test_set_get_uploaded_length() {
        let group = RequestGroup::new(
            GroupId::new(6),
            vec!["http://seed.test/file.torrent".to_string()],
            DownloadOptions::default(),
        );

        // Test default
        assert_eq!(group.get_uploaded_length(), 0);

        // Test set/get
        group.set_uploaded_length(512);
        assert_eq!(group.get_uploaded_length(), 512);

        // Test large value
        group.set_uploaded_length(u64::MAX / 2);
        assert_eq!(group.get_uploaded_length(), u64::MAX / 2);
    }

    #[test]
    fn test_set_get_download_speed_cached() {
        let group = RequestGroup::new(
            GroupId::new(7),
            vec!["http://speed.test/large.file".to_string()],
            DownloadOptions::default(),
        );

        // Test default
        assert_eq!(group.get_download_speed_cached(), 0);

        // Test realistic download speed (e.g., 5 MB/s = 5242880 bytes/s)
        group.set_download_speed_cached(5242880);
        assert_eq!(group.get_download_speed_cached(), 5242880);

        // Test speed update (simulating periodic updates)
        group.set_download_speed_cached(10485760); // 10 MB/s
        assert_eq!(group.get_download_speed_cached(), 10485760);
    }

    #[test]
    fn test_download_options_choking_config_defaults() {
        // New DownloadOptions should have None for choking algorithm fields (opt-in)
        let opts = DownloadOptions::default();

        assert!(
            opts.bt_max_upload_slots.is_none(),
            "bt_max_upload_slots should default to None"
        );
        assert!(
            opts.bt_optimistic_unchoke_interval.is_none(),
            "bt_optimistic_unchoke_interval should default to None"
        );
        assert!(
            opts.bt_snubbed_timeout.is_none(),
            "bt_snubbed_timeout should default to None"
        );
    }

    #[test]
    fn test_download_options_choking_config_custom() {
        // Verify that custom choking config values can be set
        let opts = DownloadOptions {
            bt_max_upload_slots: Some(8),
            bt_optimistic_unchoke_interval: Some(15),
            bt_snubbed_timeout: Some(45),
            ..DownloadOptions::default()
        };

        assert_eq!(opts.bt_max_upload_slots, Some(8));
        assert_eq!(opts.bt_optimistic_unchoke_interval, Some(15));
        assert_eq!(opts.bt_snubbed_timeout, Some(45));
    }

    #[test]
    fn test_download_options_choking_config_clone() {
        // Verify choking config fields are preserved through Clone
        let opts = DownloadOptions {
            bt_max_upload_slots: Some(6),
            bt_optimistic_unchoke_interval: Some(20),
            bt_snubbed_timeout: Some(90),
            ..DownloadOptions::default()
        };

        let cloned = opts.clone();

        assert_eq!(cloned.bt_max_upload_slots, Some(6));
        assert_eq!(cloned.bt_optimistic_unchoke_interval, Some(20));
        assert_eq!(cloned.bt_snubbed_timeout, Some(90));
    }
    
    // ==================== BT Metadata Tests ====================
    
    #[test]
    fn test_bt_metadata_defaults() {
        let group = RequestGroup::new(
            GroupId::new(8),
            vec!["http://example.com/file.zip".to_string()],
            DownloadOptions::default(),
        );
        
        // Non-BT downloads should have 0/None defaults
        assert_eq!(group.get_bt_num_pieces(), 0, "bt_num_pieces should default to 0");
        assert_eq!(group.get_bt_piece_length(), 0, "bt_piece_length should default to 0");
        assert_eq!(group.get_bt_info_hash_hex(), None, "bt_info_hash_hex should default to None");
    }
    
    #[test]
    fn test_set_bt_metadata() {
        let group = RequestGroup::new(
            GroupId::new(9),
            vec!["magnet:?xt=urn:btih:abc123def456".to_string()],
            DownloadOptions::default(),
        );
        
        // Set BT metadata
        group.set_bt_metadata(100, 262144, "abc123def456789012345678901234567890abcd".to_string());
        
        // Verify values
        assert_eq!(group.get_bt_num_pieces(), 100);
        assert_eq!(group.get_bt_piece_length(), 262144); // 256KB
        assert_eq!(
            group.get_bt_info_hash_hex(),
            Some("abc123def456789012345678901234567890abcd".to_string())
        );
    }
    
    #[test]
    fn test_bt_metadata_update() {
        let group = RequestGroup::new(
            GroupId::new(10),
            vec!["bt://test.torrent".to_string()],
            DownloadOptions::default(),
        );
        
        // Initial set
        group.set_bt_metadata(50, 16384, "first_hash".to_string());
        assert_eq!(group.get_bt_num_pieces(), 50);
        
        // Update with new values
        group.set_bt_metadata(200, 524288, "updated_hash".to_string());
        assert_eq!(group.get_bt_num_pieces(), 200);
        assert_eq!(group.get_bt_piece_length(), 524288);
        assert_eq!(group.get_bt_info_hash_hex(), Some("updated_hash".to_string()));
    }
    
    #[tokio::test]
    async fn test_bt_info_hash_hex_async() {
        let group = RequestGroup::new(
            GroupId::new(11),
            vec!["magnet:?xt=urn:btih:test".to_string()],
            DownloadOptions::default(),
        );
        
        // Set via blocking method
        group.set_bt_metadata(10, 1024, "async_test_hash".to_string());
        
        // Read via async method
        let hash = group.get_bt_info_hash_hex_async().await;
        assert_eq!(hash, Some("async_test_hash".to_string()));
    }
}
