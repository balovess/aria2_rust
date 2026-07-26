use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tracing::{debug, info};

use crate::download::DownloadContext;
use crate::error::Result;
use crate::rate_limiter::RateLimiter;
use crate::segment::Segment;
use crate::util::rwlock_ext::RwLockRecover;

use super::progress::AtomicProgress;
use super::status::DownloadStatus;
use super::group_id::GroupId;
use super::options::DownloadOptions;
use super::halt_reason::{DownloadControlFlags, HaltReason};
use super::result_code::{DownloadResultCode, DownloadResult};

pub struct RequestGroup {
    pub(super) gid: GroupId,
    pub(super) uris: Vec<String>,
    pub(super) options: Arc<DownloadOptions>,
    pub(super) status: std::sync::RwLock<DownloadStatus>,
    pub(super) segments: std::sync::RwLock<Vec<Segment>>,
    pub(super) start_time: std::sync::RwLock<Option<std::time::Instant>>,
    pub(super) end_time: std::sync::RwLock<Option<std::time::Instant>>,
    /// Lock-free progress counters shared via `Arc` so the hot-path download
    /// code can update progress without acquiring the outer `RwLock`.
    pub progress: Arc<AtomicProgress>,
    pub uploaded_length: AtomicU64,
    pub bt_bitfield: std::sync::RwLock<Option<Vec<u8>>>,

    /// Download context — central metadata (file entries, piece hashes, attributes).
    /// In C++ aria2, `RequestGroup` owns `shared_ptr<DownloadContext> dctx_`.
    /// For non-BT downloads this is created during URI resolution; for BT
    /// downloads it is created from TorrentMeta in BtDownloadCommand.
    /// `None` until the download engine populates it.
    pub download_context: std::sync::RwLock<Option<Arc<DownloadContext>>>,

    // BT metadata fields (for session persistence enhancement)
    /// Number of pieces in the torrent (0 for non-BT downloads)
    pub bt_num_pieces: AtomicU32,
    /// Size of each piece in bytes (0 for non-BT downloads)
    pub bt_piece_length: AtomicU32,
    /// Info hash hex string for torrent identification (None for non-BT downloads)
    pub bt_info_hash_hex: std::sync::RwLock<Option<String>>,

    /// Handle to the download's `RateLimiter` so that runtime option updates
    /// (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    /// `None` until the download engine wires up a `ThrottledWriter`.
    /// Uses its own `RwLock` (independent of the `RequestGroupMan` write lock)
    /// so that `set_rate_limiter` can take `&self`.
    pub rate_limiter: std::sync::RwLock<Option<RateLimiter>>,

    // ── Lifecycle control flags (C++ haltRequested_/forceHaltRequested_/pauseRequested_) ──

    /// Atomic control flags for halt/pause/force-halt/restart signals.
    /// Checked by hot download loops without acquiring the `RwLock` on status.
    /// Separates *intent* (halt requested) from *observed status* (Paused, etc.).
    pub control_flags: DownloadControlFlags,

    /// Reason why the download was halted. Used to determine the
    /// `DownloadResultCode` reported to RPC consumers.
    pub halt_reason: std::sync::RwLock<HaltReason>,

    /// Last error code recorded for this download.
    /// Used by `create_download_result()` to produce structured result codes.
    pub last_error_code: std::sync::RwLock<DownloadResultCode>,

    /// Last error message recorded for this download.
    pub last_error_message: std::sync::RwLock<String>,
}

impl RequestGroup {
    pub fn new(gid: GroupId, uris: Vec<String>, options: DownloadOptions) -> Self {
        info!("Creating request group #{}", gid.value());

        RequestGroup {
            gid,
            uris,
            options: Arc::new(options),
            status: std::sync::RwLock::new(DownloadStatus::Waiting),
            segments: std::sync::RwLock::new(Vec::new()),
            start_time: std::sync::RwLock::new(None),
            end_time: std::sync::RwLock::new(None),
            progress: Arc::new(AtomicProgress::new()),
            uploaded_length: AtomicU64::new(0),
            bt_bitfield: std::sync::RwLock::new(None),
            download_context: std::sync::RwLock::new(None),

            // Initialize BT metadata fields (default to 0/None for non-BT downloads)
            bt_num_pieces: AtomicU32::new(0),
            bt_piece_length: AtomicU32::new(0),
            bt_info_hash_hex: std::sync::RwLock::new(None),

            // Rate limiter is wired up later by the download engine via
            // `set_rate_limiter` once a `ThrottledWriter` is constructed.
            rate_limiter: std::sync::RwLock::new(None),

            // Lifecycle control flags — all cleared initially
            control_flags: DownloadControlFlags::new(),
            halt_reason: std::sync::RwLock::new(HaltReason::None),
            last_error_code: std::sync::RwLock::new(DownloadResultCode::UnknownError),
            last_error_message: std::sync::RwLock::new(String::new()),
        }
    }

    pub fn start(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut start_time = self.start_time.recover_mut();

        *status = DownloadStatus::Active;
        *start_time = Some(std::time::Instant::now());

        info!("Starting download task #{}", self.gid.value());
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Active | DownloadStatus::Waiting) {
            *status = DownloadStatus::Paused;
            self.control_flags.request_pause();
            info!("Pausing download task #{}", self.gid.value());
        }

        Ok(())
    }

    /// Force-pause: set both pause and force-pause flags so the download loop
    /// aborts in-flight commands immediately. Mirrors C++ `forcePauseRequested`.
    pub fn force_pause(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Active | DownloadStatus::Waiting) {
            *status = DownloadStatus::Paused;
            self.control_flags.request_force_pause();
            info!("Force-pausing download task #{}", self.gid.value());
        }

        Ok(())
    }

    /// Resume a paused download. Clears pause flags and transitions to Waiting.
    pub fn resume(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Paused) {
            *status = DownloadStatus::Waiting;
            self.control_flags.clear_pause();
            info!("Resuming download task #{}", self.gid.value());
        }

        Ok(())
    }

    pub fn remove(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut end_time = self.end_time.recover_mut();

        *status = DownloadStatus::Removed;
        *end_time = Some(std::time::Instant::now());
        *self.halt_reason.recover_mut() = HaltReason::UserRequest;

        info!("Removing download task #{}", self.gid.value());
        Ok(())
    }

    /// Request a graceful halt (let in-flight chunks finish).
    /// Mirrors C++ `setHaltRequested(true, reason)`.
    pub fn request_halt(&self, reason: HaltReason) {
        self.control_flags.request_halt();
        *self.halt_reason.recover_mut() = reason;
        info!(gid = self.gid.value(), ?reason, "Halt requested");
    }

    /// Request a forced halt (abort immediately, even mid-write).
    /// Mirrors C++ `setForceHaltRequested(true, reason)`.
    pub fn request_force_halt(&self, reason: HaltReason) {
        self.control_flags.request_force_halt();
        *self.halt_reason.recover_mut() = reason;
        info!(gid = self.gid.value(), ?reason, "Force halt requested");
    }

    pub fn complete(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut end_time = self.end_time.recover_mut();

        let total = self.progress.total_length();
        *status = DownloadStatus::Complete;
        *end_time = Some(std::time::Instant::now());
        self.progress.set_completed_length(total);

        info!("Completing download task #{}", self.gid.value());
        Ok(())
    }

    pub fn error(&mut self, err: impl Into<String>) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut end_time = self.end_time.recover_mut();

        *status = DownloadStatus::Error(err.into());
        *end_time = Some(std::time::Instant::now());

        debug!("Download task #{} encountered error", self.gid.value());
        Ok(())
    }

    pub fn status(&self) -> DownloadStatus {
        self.status.recover().clone()
    }

    /// Non-blocking check whether this group has been marked as `Removed`.
    ///
    /// Uses `try_read` on the inner status lock so it is safe to call from
    /// hot download loops without risking lock contention or deadlock. When
    /// the lock is contended the method returns `false` (treats the task as
    /// still running); the caller will re-check on the next iteration.
    ///
    /// This is the primary signal used by `DownloadCommand` and the
    /// underlying downloaders to detect that `aria2.remove` /
    /// `aria2.forceRemove` has been invoked: `RequestGroupMan::remove_group`
    /// sets the status to `Removed`, and the running download observes it
    /// here and aborts promptly.
    pub fn is_removed(&self) -> bool {
        match self.status.try_read() {
            Ok(guard) => matches!(*guard, DownloadStatus::Removed),
            Err(_) => false,
        }
    }

    /// Check whether this group has been paused (non-blocking).
    /// Used by downloaders to detect `aria2.pause` / `aria2.forcePause`.
    pub fn is_paused_flag(&self) -> bool {
        match self.status.try_read() {
            Ok(guard) => matches!(*guard, DownloadStatus::Paused),
            Err(_) => false,
        }
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

    /// Cheap clone of the options `Arc` — O(1) refcount bump instead of
    /// deep-cloning all `Vec<String>` fields.
    pub fn options_arc(&self) -> Arc<DownloadOptions> {
        Arc::clone(&self.options)
    }

    /// Store a handle to the download's `RateLimiter` so that runtime option
    /// updates (e.g. via `aria2.changeOption`) can dynamically adjust the rate.
    /// The `RateLimiter` is `Clone` and shares `Arc<RateLimiterInner>`, so both
    /// the `ThrottledWriter` and `RequestGroup` see the same token buckets.
    ///
    /// Takes `&self` because `rate_limiter` has its own `RwLock`, independent
    /// of the `RequestGroupMan` outer write lock.
    pub fn set_rate_limiter(&self, limiter: RateLimiter) {
        *self.rate_limiter.recover_mut() = Some(limiter);
    }

    /// Update a single runtime-changeable option by key (using aria2's
    /// kebab-case option names, e.g. `"max-download-limit"`).
    ///
    /// Returns `true` if the option was recognized and updated, `false` if the
    /// key is not a runtime-changeable option (see [`super::options::RUNTIME_CHANGEABLE_OPTIONS`]).
    ///
    /// Takes `&mut self` because `options` is a plain field — the caller
    /// (`RequestGroupMan`) holds the outer `Arc<RwLock<RequestGroup>>` write
    /// lock. For `max-download-limit` / `max-upload-limit`, the stored
    /// `RateLimiter` (if any) is also updated so the change takes effect
    /// immediately on the live download.
    pub fn update_option(&mut self, key: &str, value: serde_json::Value) -> bool {
        // Use Arc::make_mut for copy-on-write: if the Arc is uniquely held,
        // this mutates in place (zero alloc); otherwise it clones the inner
        // value first (rare — only when options_arc() was called before).
        let opts = Arc::make_mut(&mut self.options);
        match key {
            "split" => {
                if let Some(v) = value.as_u64() {
                    opts.split = Some(v as u16);
                    tracing::warn!(
                        new_split = v,
                        "split changed but will take effect on download restart/retry, \
                         not mid-download (current segments unchanged)"
                    );
                }
                true
            }
            "max-download-limit" => {
                let rate = value.as_u64();
                opts.max_download_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_download_rate(rate);
                }
                true
            }
            "max-upload-limit" => {
                let rate = value.as_u64();
                opts.max_upload_limit = rate;
                if let Some(ref limiter) = *self.rate_limiter.recover() {
                    limiter.set_upload_rate(rate);
                }
                true
            }
            "max-retries" => {
                if let Some(v) = value.as_u64() {
                    opts.max_retries = v as u32;
                }
                true
            }
            "retry-wait" => {
                if let Some(v) = value.as_u64() {
                    opts.retry_wait = v;
                }
                true
            }
            "header" => {
                // Accept both a JSON array of strings and a newline-separated
                // string (matching aria2's wire format).
                match &value {
                    serde_json::Value::Array(arr) => {
                        opts.header = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect();
                    }
                    serde_json::Value::String(s) => {
                        opts.header = s
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
                opts.user_agent = value.as_str().map(|s| s.to_string());
                true
            }
            "referer" => {
                opts.referer = value.as_str().map(|s| s.to_string());
                true
            }
            "max-connection-per-server" => {
                if let Some(v) = value.as_u64() {
                    opts.max_connection_per_server = Some(v as u16);
                }
                true
            }
            "bt-max-upload-slots" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_max_upload_slots = Some(v as u32);
                }
                true
            }
            "bt-snubbed-timeout" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_snubbed_timeout = Some(v);
                }
                true
            }
            "bt-optimistic-unchoke-interval" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_optimistic_unchoke_interval = Some(v);
                }
                true
            }
            "bt-endgame-threshold" => {
                if let Some(v) = value.as_u64() {
                    opts.bt_endgame_threshold = v as u32;
                }
                true
            }
            "seed-time" => {
                if let Some(v) = value.as_f64() {
                    opts.seed_time = Some(v);
                }
                true
            }
            "seed-ratio" => {
                if let Some(v) = value.as_f64() {
                    opts.seed_ratio = Some(v);
                }
                true
            }
            "dir" => {
                opts.dir = value.as_str().map(|s| s.to_string());
                true
            }
            "out" => {
                opts.out = value.as_str().map(|s| s.to_string());
                true
            }
            "file-allocation" => {
                if let Some(s) = value.as_str() {
                    opts.file_allocation = Some(s.to_string());
                }
                true
            }
            "mmap-threshold" => {
                opts.mmap_threshold = value.as_u64();
                true
            }
            "secure-falloc" => {
                opts.secure_falloc = value.as_bool().unwrap_or(false);
                true
            }
            "checksum" => {
                if let Some(s) = value.as_str() {
                    if let Some((algo, hash)) = s.split_once('=') {
                        opts.checksum = Some((algo.to_string(), hash.to_string()));
                    }
                }
                true
            }
            "cookie-file" => {
                opts.cookie_file = value.as_str().map(|s| s.to_string());
                true
            }
            "cookies" => {
                opts.cookies = value.as_str().map(|s| s.to_string());
                true
            }
            "bt-force-encrypt" => {
                opts.bt_force_encrypt = value.as_bool().unwrap_or(false);
                true
            }
            "bt-require-crypto" => {
                opts.bt_require_crypto = value.as_bool().unwrap_or(false);
                true
            }
            "enable-dht" => {
                opts.enable_dht = value.as_bool().unwrap_or(true);
                true
            }
            "dht-listen-port" => {
                opts.dht_listen_port = value.as_u64().map(|v| v as u16);
                true
            }
            "dht-entry-point" => {
                match &value {
                    serde_json::Value::String(s) => {
                        opts.dht_entry_point = Some(vec![s.to_string()]);
                    }
                    serde_json::Value::Array(arr) => {
                        opts.dht_entry_point = Some(
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect(),
                        );
                    }
                    _ => {}
                }
                true
            }
            "enable-public-trackers" => {
                opts.enable_public_trackers = value.as_bool().unwrap_or(true);
                true
            }
            "bt-piece-selection-strategy" => {
                if let Some(s) = value.as_str() {
                    opts.bt_piece_selection_strategy = s.to_string();
                }
                true
            }
            "bt-prioritize-piece" => {
                if let Some(s) = value.as_str() {
                    opts.bt_prioritize_piece = s.to_string();
                }
                true
            }
            "enable-utp" => {
                opts.enable_utp = value.as_bool().unwrap_or(false);
                true
            }
            "utp-listen-port" => {
                opts.utp_listen_port = value.as_u64().map(|v| v as u16);
                true
            }
            "dht-file-path" => {
                opts.dht_file_path = value.as_str().map(|s| s.to_string());
                true
            }
            "http-proxy" => {
                opts.http_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "all-proxy" => {
                opts.all_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "https-proxy" => {
                opts.https_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "ftp-proxy" => {
                opts.ftp_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            "no-proxy" => {
                opts.no_proxy = value.as_str().map(|s| s.to_string());
                true
            }
            _ => false,
        }
    }

    pub fn total_length(&self) -> u64 {
        self.progress.total_length()
    }

    pub fn set_total_length(&self, length: u64) {
        self.progress.set_total_length(length);
        debug!("Setting total length: {} bytes", length);
    }

    pub fn completed_length(&self) -> u64 {
        self.progress.completed_length()
    }

    pub fn update_completed_length(&self, length: u64) {
        self.progress.set_completed_length(length);
    }

    pub fn update_progress(&self, completed_length: u64) {
        self.progress.set_completed_length(completed_length);
    }

    pub fn progress(&self) -> f64 {
        let total = self.progress.total_length();
        let completed = self.progress.completed_length();

        if total == 0 {
            0.0
        } else {
            (completed as f64 / total as f64) * 100.0
        }
    }

    pub fn download_speed(&self) -> u64 {
        self.progress.download_speed()
    }

    pub fn upload_speed(&self) -> u64 {
        self.progress.upload_speed()
    }

    pub fn update_speed(&self, dl_speed: u64, ul_speed: u64) {
        self.progress.set_download_speed(dl_speed);
        self.progress.set_upload_speed(ul_speed);
    }

    pub fn add_segment(&mut self, segment: Segment) {
        let mut segments = self.segments.recover_mut();
        segments.push(segment);
        debug!("Adding segment, current segments: {}", segments.len());
    }

    pub fn segments(&self) -> Vec<Segment> {
        self.segments.recover().clone()
    }

    pub fn elapsed_time(&self) -> Option<std::time::Duration> {
        let start = *self.start_time.recover();
        start.map(|t| t.elapsed())
    }

    pub fn eta(&self) -> Option<std::time::Duration> {
        let speed = self.progress.download_speed();
        let total = self.progress.total_length();
        let completed = self.progress.completed_length();
        let remaining = total.saturating_sub(completed);

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
        self.progress.set_completed_length(val);
    }

    /// Get completed length using atomic load (lock-free)
    pub fn get_completed_length(&self) -> u64 {
        self.progress.completed_length()
    }

    /// Set total length using atomic store (lock-free)
    pub fn set_total_length_atomic(&self, val: u64) {
        self.progress.set_total_length(val);
    }

    /// Get total length using atomic load (lock-free)
    pub fn get_total_length_atomic(&self) -> u64 {
        self.progress.total_length()
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
        self.progress.set_download_speed(val);
    }

    /// Get download speed from cache using atomic load (lock-free)
    pub fn get_download_speed_cached(&self) -> u64 {
        self.progress.download_speed()
    }

    /// Set upload speed cache using atomic store (lock-free)
    pub fn set_upload_speed_cached(&self, val: u64) {
        self.progress.set_upload_speed(val);
    }

    /// Get upload speed from cache using atomic load (lock-free)
    pub fn get_upload_speed_cached(&self) -> u64 {
        self.progress.upload_speed()
    }

    /// Set BT bitfield (sync, uses std::sync::RwLock)
    pub fn set_bt_bitfield(&self, bf: Option<Vec<u8>>) {
        *self.bt_bitfield.recover_mut() = bf;
    }

    /// Get BT bitfield (sync, uses std::sync::RwLock)
    pub fn get_bt_bitfield(&self) -> Option<Vec<u8>> {
        self.bt_bitfield.recover().clone()
    }

    /// Set resume offset for HTTP/FTP range request resumption
    pub fn set_resume_offset(&self, offset: u64) {
        // Store resume offset in completed_length so the download engine
        // knows where to resume from
        self.progress.set_completed_length(offset);
    }

    // BT metadata methods (for session persistence enhancement)

    /// Set BT metadata fields (num_pieces, piece_length, info_hash_hex)
    /// Called by BtDownloadCommand after parsing TorrentMeta
    pub fn set_bt_metadata(&self, num_pieces: u32, piece_length: u32, info_hash_hex: String) {
        self.bt_num_pieces.store(num_pieces, Ordering::Relaxed);
        self.bt_piece_length.store(piece_length, Ordering::Relaxed);
        // Use std::sync::RwLock for non-async access
        *self.bt_info_hash_hex.recover_mut() = Some(info_hash_hex);
    }

    /// Get number of pieces (lock-free atomic read)
    pub fn get_bt_num_pieces(&self) -> u32 {
        self.bt_num_pieces.load(Ordering::Relaxed)
    }

    /// Get piece length (lock-free atomic read)
    pub fn get_bt_piece_length(&self) -> u32 {
        self.bt_piece_length.load(Ordering::Relaxed)
    }

    /// Get info hash hex string (blocking read for non-async contexts)
    pub fn get_bt_info_hash_hex(&self) -> Option<String> {
        self.bt_info_hash_hex.recover().clone()
    }

    // -----------------------------------------------------------------------
    // DownloadContext accessors
    // -----------------------------------------------------------------------

    /// Get a shared reference to the `DownloadContext`, if set.
    ///
    /// Returns `None` if the download context has not been initialized yet
    /// (e.g. before torrent metadata is parsed for BT downloads).
    pub fn get_download_context(&self) -> Option<Arc<DownloadContext>> {
        self.download_context.recover().clone()
    }

    /// Set the `DownloadContext` for this download.
    ///
    /// In C++ aria2, `RequestGroup` always has a `DownloadContext` (created in
    /// the constructor or by BtSetup). In Rust, we set it lazily — for BT
    /// downloads this happens in `BtDownloadCommand::new()` after parsing
    /// TorrentMeta.
    pub fn set_download_context(&self, ctx: Arc<DownloadContext>) {
        *self.download_context.recover_mut() = Some(ctx);
    }

    // -----------------------------------------------------------------------
    // Lifecycle control flag accessors
    // -----------------------------------------------------------------------

    /// Non-blocking check whether a graceful halt has been requested.
    ///
    /// The download loop should call this on each iteration; if `true`,
    /// finish writing the current chunk and then stop.
    pub fn is_halt_requested(&self) -> bool {
        self.control_flags.is_halt_requested()
    }

    /// Non-blocking check whether a forced halt has been requested.
    ///
    /// If `true`, the download loop should abort immediately, even mid-write.
    pub fn is_force_halt_requested(&self) -> bool {
        self.control_flags.is_force_halt_requested()
    }

    /// Non-blocking check whether a pause has been requested.
    pub fn is_pause_requested(&self) -> bool {
        self.control_flags.is_pause_requested()
    }

    /// Non-blocking check whether a forced pause has been requested.
    pub fn is_force_pause_requested(&self) -> bool {
        self.control_flags.is_force_pause_requested()
    }

    /// Non-blocking check whether a restart has been requested.
    pub fn is_restart_requested(&self) -> bool {
        self.control_flags.is_restart_requested()
    }

    /// Request a restart (stop current download and re-queue).
    pub fn request_restart(&self) {
        self.control_flags.request_restart();
        info!(gid = self.gid.value(), "Restart requested");
    }

    /// Get the current halt reason.
    pub fn get_halt_reason(&self) -> HaltReason {
        *self.halt_reason.recover()
    }

    /// Record an error code and message for this download.
    pub fn set_last_error(&self, code: DownloadResultCode, message: impl Into<String>) {
        *self.last_error_code.recover_mut() = code;
        *self.last_error_message.recover_mut() = message.into();
    }

    /// Get the last recorded error code.
    pub fn get_last_error_code(&self) -> DownloadResultCode {
        *self.last_error_code.recover()
    }

    /// Get the last recorded error message.
    pub fn get_last_error_message(&self) -> String {
        self.last_error_message.recover().clone()
    }

    // -----------------------------------------------------------------------
    // DownloadResult creation
    // -----------------------------------------------------------------------

    /// Create a structured `DownloadResult` for this download.
    ///
    /// Mirrors C++ `RequestGroup::downloadResult()` logic:
    /// - If download is complete and no checksum verification pending → Finished
    /// - If halt reason is UserRequest → Removed
    /// - If last error is unknown and halt reason is ShutdownSignal → InProgress
    /// - If last error is unknown → UnknownError
    /// - Otherwise → the recorded last error code
    pub fn create_download_result(&self) -> DownloadResult {
        let status = self.status.recover().clone();
        let halt_reason = *self.halt_reason.recover();
        let last_code = *self.last_error_code.recover();
        let last_msg = self.last_error_message.recover().clone();

        match status {
            DownloadStatus::Complete => {
                // Check if checksum verification is still pending
                let checksum_pending = self
                    .download_context
                    .recover()
                    .as_ref()
                    .map(|ctx| ctx.is_checksum_verification_pending())
                    .unwrap_or(false);

                if checksum_pending {
                    DownloadResult::error(
                        DownloadResultCode::ChecksumError,
                        "Checksum verification pending",
                    )
                } else {
                    DownloadResult::finished()
                }
            }
            DownloadStatus::Removed => {
                DownloadResult::removed()
            }
            DownloadStatus::Paused => {
                DownloadResult::paused()
            }
            DownloadStatus::Error(_) => {
                // Use structured error code if available
                if last_code != DownloadResultCode::UnknownError {
                    DownloadResult::error(last_code, last_msg)
                } else {
                    DownloadResult::error(DownloadResultCode::UnknownError, "Unknown error")
                }
            }
            DownloadStatus::Waiting | DownloadStatus::Active => {
                // Download was interrupted (e.g. by shutdown)
                match halt_reason {
                    HaltReason::ShutdownSignal => DownloadResult::in_progress(),
                    HaltReason::UserRequest => DownloadResult::removed(),
                    HaltReason::None => {
                        if last_code != DownloadResultCode::UnknownError {
                            DownloadResult::error(last_code, last_msg)
                        } else {
                            DownloadResult::in_progress()
                        }
                    }
                }
            }
        }
    }
}
