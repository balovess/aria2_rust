mod constructor;
mod execute;
mod finalize;
mod in_memory;
mod lifecycle;
mod metadata_request;
mod tail_reclaim;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::engine::command::ProgressMessage;
use crate::http::HttpRequestPolicy;
use crate::http::cookie::CookieStorage;
use crate::http::socks_connector::NoProxyMatcher;
use crate::network::OutboundNetworkPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::perf_monitor::{AtomicMetrics, PerformanceMonitor};
/// Core download command that handles HTTP/HTTPS file downloads.
///
/// Supports both sequential and concurrent (range-based) download strategies,
/// with automatic resume, cookie management, proxy configuration, and
/// checksum verification.
pub struct DownloadCommand {
    pub(super) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters -- avoids RwLock on the hot path.
    pub(super) progress: Arc<AtomicProgress>,
    pub(super) client: Arc<reqwest::Client>,
    /// Independent transport pools used only for concurrent HTTP ranges.
    pub(super) range_clients: Arc<Vec<reqwest::Client>>,
    pub(super) outbound_network_policy: Arc<OutboundNetworkPolicy>,
    pub(super) output_path: std::path::PathBuf,
    /// Whether the filename came from an explicit `--out`/metadata name.
    /// Implicit HTTP names may be replaced by response metadata before I/O.
    pub(super) output_name_explicit: bool,
    /// Whether the output path has already gone through collision resolution.
    /// Mirror failover reuses this resolved path after the prior attempt
    /// releases its temporary registry claim.
    pub(super) output_path_resolved: bool,
    pub(super) started: bool,
    pub(super) completed: bool,
    pub(super) completed_bytes: u64,
    pub(super) file_allocation: String,
    pub(super) mmap_threshold: u64,
    pub(super) secure_falloc: bool,
    /// `--check-integrity`: verify existing data against context piece hashes
    /// before downloading (C++ `CheckIntegrityMan`). Only meaningful when the
    /// DownloadContext carries piece hashes (e.g. Metalink).
    pub(super) check_integrity: bool,
    pub(super) cookie_storage: Arc<CookieStorage>,
    pub(super) cookie_file: Option<String>,
    pub(super) no_proxy_matcher: Option<NoProxyMatcher>,
    pub(super) stat_man: Arc<ServerStatMan>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` / segment download loops
    /// so that all concurrent downloads share a single bandwidth ceiling.
    pub(super) global_limiter: Option<RateLimiter>,
    pub(super) perf_monitor: Option<Arc<PerformanceMonitor>>,
    pub(super) atomic_metrics: Arc<AtomicMetrics>,
    pub(super) request_policy: HttpRequestPolicy,
    pub(super) progress_sender: Option<mpsc::Sender<ProgressMessage>>,
    pub(super) progress_receiver: Option<mpsc::Receiver<ProgressMessage>>,
    pub(super) progress_aggregator_handle: Option<tokio::task::JoinHandle<()>>,

    // ── Tail reclaim progress tracking ─────────────────────────────────
    // Mirrors C++ DownloadCommand fields:
    //   lastTailReclaimSessionDownloadLength_, tailReclaimLastProgress_,
    //   startupIdleTime_, lowestDownloadSpeedLimit_
    //
    // These fields track when data was last received so that the tail
    // reclaim policy can detect stalled connections.  In C++ these are
    // updated on every data chunk via updateTailReclaimProgress().  In Rust
    // they are updated via update_tail_reclaim_progress() which reads from
    // the lock-free AtomicProgress counter.
    /// Completed length at the last time progress was detected.
    /// Mirrors C++ `lastTailReclaimSessionDownloadLength_`.
    pub(super) last_tail_reclaim_session_download_length: u64,

    /// Timestamp of the last time progress was detected.
    /// Mirrors C++ `tailReclaimLastProgress_`.
    pub(super) tail_reclaim_last_progress: Instant,

    /// Stall threshold — if no progress for this duration, the connection
    /// is considered stalled.  Mirrors C++ `startupIdleTime_`.
    /// Defaults to 10 seconds (C++ `PREF_STARTUP_IDLE_TIME` default).
    pub(super) startup_idle_time: Duration,

    /// Lowest download speed limit in bytes/sec.  Downloads slower than
    /// this are aborted.  Mirrors C++ `lowestDownloadSpeedLimit_`.
    /// 0 means no limit.
    pub(super) lowest_speed_limit: u64,
}
